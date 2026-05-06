//! Multi-backend terminal architecture
//!
//! Babel was born as a kitty-only daemon. This module generalizes it into a
//! backend-agnostic system where kitty, tmux, and future terminal multiplexers
//! are equal citizens behind the [`TerminalBackend`] trait.

//!
//! Key types:
//! - [`Pane`] — backend-neutral pane descriptor (replaces the old `KittyPane`)
//! - [`PaneExtras`] — backend-specific metadata that doesn't fit the common schema
//! - [`BackendInstance`] — a discovered terminal instance (one kitty socket, one tmux server)
//! - [`BackendRegistry`] — routes operations to the correct backend by connection string
//!
//! Connection strings are the universal address scheme:
//! - Kitty: `unix:/run/user/1000/kitty.sock-<pid>`
//! - Tmux: `tmux:/tmp/tmux-1000/default`
//!
//! Each backend implementation lives in its own submodule (e.g. `kitty_backend`).
//! The registry holds `Arc<dyn TerminalBackend>` so backends are shared across
//! async tasks without lifetime gymnastics.

pub mod kitty;
pub mod tmux;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Pane — the universal terminal pane descriptor
// ---------------------------------------------------------------------------

/// A terminal pane as seen by babel, regardless of which backend owns it.
///
/// This is the single pane type that flows through events, indicators, paint,
/// and the CLI. Backend-specific details live in [`PaneExtras`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pane {
    /// Backend connection string.
    /// Kitty: `"unix:/run/user/1000/kitty.sock-12345"`,
    /// tmux: `"tmux:/tmp/tmux-1000/default"`
    pub connection: String,
    /// Pane ID within that backend instance. Kitty: window id, tmux: %N numeric.
    pub id: u64,
    pub title: String,
    pub cwd: PathBuf,
    pub is_focused: bool,
    pub is_active: bool,
    pub foreground_processes: Vec<ForegroundProcess>,
    pub user_vars: HashMap<String, String>,
    /// X11/Wayland window ID. Some for kitty (per-pane), None or shared for tmux.
    pub platform_window_id: Option<u64>,
    pub extras: PaneExtras,
}

impl Pane {
    /// Construct a [`PaneAddr`](crate::model::PaneAddr) from this pane's connection + id.
    ///
    /// PaneAddr currently uses `socket` for its first field — we map `connection` to that
    /// since PaneAddr predates the multi-backend generalization and will be renamed later.
    pub fn addr(&self) -> crate::model::PaneAddr {
        crate::model::PaneAddr::new(&self.connection, self.id)
    }

    // === Backend-specific accessors ===
    // These bridge the gap between the old KittyPane flat struct and the new
    // PaneExtras-based design. Consumers that only care about kitty can use these
    // instead of pattern-matching on extras every time.

    /// Kitty OS window ID (internal kitty concept, not the X11 window).
    /// Returns None for non-kitty backends.
    pub fn os_window_id(&self) -> Option<u64> {
        match &self.extras {
            PaneExtras::Kitty { os_window_id, .. } => Some(*os_window_id),
            _ => None,
        }
    }

    /// Kitty screen geometry (absolute pixel coordinates from newer kitty).
    /// Returns None for non-kitty backends or older kitty versions.
    pub fn screen(&self) -> Option<&ScreenGeometry> {
        match &self.extras {
            PaneExtras::Kitty { screen, .. } => screen.as_ref(),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// PaneExtras — backend-specific pane metadata
// ---------------------------------------------------------------------------

/// Backend-specific data that doesn't fit the common [`Pane`] schema.
///
/// Each variant carries the extra information a specific backend needs
/// to fully describe a pane. Consumers that don't care can ignore it;
/// consumers that need it (e.g. kitty border coloring) can pattern-match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneExtras {
    Kitty {
        os_window_id: u64,
        screen: Option<ScreenGeometry>,
    },
    Tmux {
        session: String,
        window_idx: u32,
    },
}

// ---------------------------------------------------------------------------
// ScreenGeometry — pixel-level pane geometry (canonical home, was in kitty.rs)
// ---------------------------------------------------------------------------

/// Pixel-level geometry for a terminal pane on screen.
/// Originally lived in `kitty.rs`; promoted here as it's useful for any
/// backend that can report pane positions (e.g. tmux with `display -p`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// ForegroundProcess — what's running inside a pane
// ---------------------------------------------------------------------------

/// A process currently in the foreground of a terminal pane.
/// Used for agent detection (is this pane running `claude`, `codex`, etc.?).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub cmdline: Vec<String>,
    pub cwd: PathBuf,
}

// ---------------------------------------------------------------------------
// BackendInstance — a discovered terminal backend instance
// ---------------------------------------------------------------------------

/// A single running instance of a terminal backend (one kitty socket, one tmux server).
///
/// Discovery produces these: each registered backend scans for its instances,
/// probes them, and returns a list with responsiveness status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendInstance {
    /// Connection string (same format as [`Pane::connection`]).
    pub connection: String,
    /// PID of the terminal process, if extractable from the connection.
    pub pid: Option<u32>,
    /// Whether this is the "current" instance (detected from env vars like $KITTY_PID).
    pub is_current: bool,
    /// Whether we successfully queried this instance.
    pub is_responsive: bool,
    /// Panes in this instance (empty if not responsive).
    pub panes: Vec<Pane>,
    /// Error message if the instance was not responsive.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// TerminalBackend — the trait all backends implement
// ---------------------------------------------------------------------------

/// The contract for a terminal multiplexer backend.
///
/// Required methods cover the operations babel needs from every backend:
/// discovery, pane IO, and metadata. Optional methods have default no-op
/// implementations for capabilities that only some backends support
/// (e.g. kitty's native border coloring).
///
/// All methods that touch the network/sockets are async. Connection strings
/// are passed explicitly so a single backend instance can manage multiple
/// terminal instances (e.g. multiple kitty sockets).
#[async_trait]
pub trait TerminalBackend: Send + Sync {
    // === Identity ===

    /// Short name for logs and diagnostics (e.g. `"kitty"`, `"tmux"`).
    fn backend_name(&self) -> &'static str;

    // === Discovery (required) ===

    /// Default connection string, if detectable (from env vars, socket scan).
    /// Returns `None` if this backend is not active on the system.
    fn default_connection(&self) -> Option<String>;

    /// Find all connection strings for instances of this backend.
    fn find_all_connections(&self) -> Vec<String>;

    /// List panes on a specific connection.
    async fn list_panes(&self, conn: &str) -> Result<Vec<Pane>>;

    /// Discover all instances with responsiveness checks.
    /// Default impl iterates `find_all_connections` and calls `list_panes` on each,
    /// but backends can override with a more efficient bulk query.
    async fn discover_instances(&self) -> Vec<BackendInstance>;

    // === Pane IO (required) ===

    async fn focus_pane(&self, conn: &str, id: u64) -> Result<()>;
    async fn send_text(&self, conn: &str, id: u64, text: &str) -> Result<()>;
    async fn get_scrollback(&self, conn: &str, id: u64) -> Result<String>;
    async fn get_recent_scrollback(&self, conn: &str, id: u64, lines: usize) -> Result<String>;
    async fn close_pane(&self, conn: &str, id: u64) -> Result<()>;

    // === Metadata (required) ===

    async fn set_meta(&self, conn: &str, id: u64, key: &str, val: &str) -> Result<()>;
    async fn set_title(&self, conn: &str, id: u64, title: &str) -> Result<()>;

    // === Visual (optional — kitty has native border coloring) ===

    async fn set_border_color(
        &self,
        _conn: &str,
        _id: u64,
        _active: &str,
        _inactive: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn reset_border_color(&self, _conn: &str, _id: u64) -> Result<()> {
        Ok(())
    }

    // === Raw layout (optional — kitty JSON tree) ===

    async fn list_panes_raw(&self, _conn: &str) -> Result<String> {
        anyhow::bail!("raw pane listing not supported by this backend")
    }

    // === Capability queries ===

    /// Whether panes have individual platform_window_ids for desktop operations.
    fn has_desktop_windows(&self) -> bool {
        false
    }

    /// Whether this backend supports native pane border coloring.
    fn has_border_coloring(&self) -> bool {
        false
    }

    /// Whether this backend supports raw layout tree capture.
    fn has_raw_layout(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// BackendRegistry — multi-backend router
// ---------------------------------------------------------------------------

/// Holds all registered backends and routes operations to the correct one.
///
/// The registry is the single point of contact for code that doesn't know
/// (or care) which backend owns a given pane. It discovers across all
/// backends, lists panes globally, and resolves connection strings.
pub struct BackendRegistry {
    backends: Vec<Arc<dyn TerminalBackend>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    pub fn register(&mut self, backend: Arc<dyn TerminalBackend>) {
        self.backends.push(backend);
    }

    /// Discover all instances across all registered backends concurrently.
    pub async fn discover_all(&self) -> Vec<BackendInstance> {
        let futures: Vec<_> = self
            .backends
            .iter()
            .map(|b| b.discover_instances())
            .collect();
        let results = futures::future::join_all(futures).await;
        results.into_iter().flatten().collect()
    }

    /// List all panes across all backends.
    ///
    /// Iterates each backend's connections and collects panes. Failures on
    /// individual connections are logged and skipped — one dead socket
    /// shouldn't prevent listing panes from healthy ones.
    pub async fn list_all_panes(&self) -> Result<Vec<Pane>> {
        let mut all = Vec::new();
        for backend in &self.backends {
            let connections = backend.find_all_connections();
            for conn in connections {
                match backend.list_panes(&conn).await {
                    Ok(panes) => all.extend(panes),
                    Err(e) => tracing::warn!(
                        backend = backend.backend_name(),
                        conn = %conn,
                        "list_panes failed: {e}"
                    ),
                }
            }
        }
        Ok(all)
    }

    /// Find the backend that owns a given connection string.
    ///
    /// Checks each backend's `find_all_connections()` to see if it claims
    /// the connection. Future optimization: prefix-based dispatch (kitty
    /// connections start with `unix:`, tmux with `tmux:`).
    pub fn backend_for(&self, connection: &str) -> Option<Arc<dyn TerminalBackend>> {
        for backend in &self.backends {
            let conns = backend.find_all_connections();
            if conns.iter().any(|c| c == connection) {
                return Some(Arc::clone(backend));
            }
        }
        None
    }

    /// Route a pane operation to the correct backend via a [`PaneAddr`](crate::model::PaneAddr).
    ///
    /// PaneAddr's `socket` field maps to a connection string. This bridge
    /// exists until PaneAddr is fully generalized to use `connection`.
    pub fn backend_for_addr(
        &self,
        addr: &crate::model::PaneAddr,
    ) -> Option<Arc<dyn TerminalBackend>> {
        self.backend_for(&addr.socket)
    }

    pub fn backends(&self) -> &[Arc<dyn TerminalBackend>] {
        &self.backends
    }

    // === PaneAddr-routed convenience methods ===
    //
    // These let daemon/core code call registry.focus(addr) instead of manually
    // resolving the backend and passing connection + id. The addr.socket field
    // is used as the connection string (will become addr.connection after rename).

    fn resolve(&self, addr: &crate::model::PaneAddr) -> Result<Arc<dyn TerminalBackend>> {
        self.backend_for(&addr.socket)
            .ok_or_else(|| anyhow::anyhow!("no backend for connection {}", addr.socket))
    }

    pub async fn focus_pane(&self, addr: &crate::model::PaneAddr) -> Result<()> {
        self.resolve(addr)?.focus_pane(&addr.socket, addr.id).await
    }

    pub async fn send_text(&self, addr: &crate::model::PaneAddr, text: &str) -> Result<()> {
        self.resolve(addr)?
            .send_text(&addr.socket, addr.id, text)
            .await
    }

    pub async fn get_scrollback(&self, addr: &crate::model::PaneAddr) -> Result<String> {
        self.resolve(addr)?
            .get_scrollback(&addr.socket, addr.id)
            .await
    }

    pub async fn get_recent_scrollback(
        &self,
        addr: &crate::model::PaneAddr,
        lines: usize,
    ) -> Result<String> {
        self.resolve(addr)?
            .get_recent_scrollback(&addr.socket, addr.id, lines)
            .await
    }

    pub async fn close_pane(&self, addr: &crate::model::PaneAddr) -> Result<()> {
        self.resolve(addr)?.close_pane(&addr.socket, addr.id).await
    }

    pub async fn set_meta(
        &self,
        addr: &crate::model::PaneAddr,
        key: &str,
        val: &str,
    ) -> Result<()> {
        self.resolve(addr)?
            .set_meta(&addr.socket, addr.id, key, val)
            .await
    }

    pub async fn set_title(&self, addr: &crate::model::PaneAddr, title: &str) -> Result<()> {
        self.resolve(addr)?
            .set_title(&addr.socket, addr.id, title)
            .await
    }

    pub async fn set_border_color(
        &self,
        addr: &crate::model::PaneAddr,
        active: &str,
        inactive: &str,
    ) -> Result<()> {
        self.resolve(addr)?
            .set_border_color(&addr.socket, addr.id, active, inactive)
            .await
    }

    pub async fn reset_border_color(&self, addr: &crate::model::PaneAddr) -> Result<()> {
        self.resolve(addr)?
            .reset_border_color(&addr.socket, addr.id)
            .await
    }

    pub async fn list_panes_raw(&self, conn: &str) -> Result<String> {
        let backend = self
            .backend_for(conn)
            .ok_or_else(|| anyhow::anyhow!("no backend for connection {}", conn))?;
        backend.list_panes_raw(conn).await
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}
