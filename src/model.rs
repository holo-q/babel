//! Shared domain model.
//!
//! This module is the refactor boundary for facts that are true across daemon,
//! core, CLI, IPC, and panel-facing code. Runtime modules may add operations,
//! but identity and activity vocabulary should live here so the rest of Babel
//! speaks one language.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::AgentKind;
use spaceship_std::agents::ActivityState;

/// Canonical live pane identity.
///
/// Kitty pane ids are unique only inside one kitty remote-control socket. A
/// live pane address must therefore carry both the socket and the pane id. This
/// is intentionally not a durable session identity: sockets are runtime
/// coordinates, while sessions are harness-owned history.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneAddr {
    /// Socket path, e.g. `unix:/run/user/1000/kitty.sock-12345`.
    pub socket: String,
    /// Pane id inside that kitty instance.
    pub id: u64,
}

impl PaneAddr {
    pub fn new(socket: impl Into<String>, id: u64) -> Self {
        Self {
            socket: socket.into(),
            id,
        }
    }

    /// Short display form for logs, e.g. `42@12345`.
    ///
    /// Multi-backend aware: strips kitty socket prefix, tmux socket dir,
    /// or zellij connection prefix to produce a compact identifier.
    pub fn short(&self) -> String {
        let sock_short = if let Some((_, pid)) = self.socket.rsplit_once("kitty.sock-") {
            pid
        } else if let Some(rest) = self.socket.strip_prefix("tmux:") {
            rest.rsplit('/').next().unwrap_or(rest)
        } else if let Some(rest) = self.socket.strip_prefix("zellij:") {
            rest
        } else {
            &self.socket
        };
        format!("{}@{}", self.id, sock_short)
    }
}

impl fmt::Display for PaneAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.socket, self.id)
    }
}

impl FromStr for PaneAddr {
    type Err = ParsePaneAddrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (socket, id) = value
            .rsplit_once(':')
            .ok_or(ParsePaneAddrError::MissingSeparator)?;
        if socket.is_empty() {
            return Err(ParsePaneAddrError::MissingSocket);
        }
        let id = id
            .parse::<u64>()
            .map_err(|_| ParsePaneAddrError::InvalidId)?;
        Ok(Self::new(socket, id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsePaneAddrError {
    MissingSeparator,
    MissingSocket,
    InvalidId,
}

impl fmt::Display for ParsePaneAddrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSeparator => write!(f, "pane address must be '<socket>:<id>'"),
            Self::MissingSocket => write!(f, "pane address socket is empty"),
            Self::InvalidId => write!(f, "pane address id must be an unsigned integer"),
        }
    }
}

impl std::error::Error for ParsePaneAddrError {}

/// A live-pane operation target carried in IPC DTOs.
///
/// `Addr` is the canonical, unambiguous form that names both the kitty socket
/// and the pane id. `Id` is a legacy shim for CLI input edges where the
/// socket has not yet been resolved; the daemon honors it by scanning every
/// known kitty socket. New callers should always produce `Addr`.
///
/// Wire form (externally tagged, snake_case):
/// ```json
/// { "addr": { "socket": "unix:...", "id": 42 } }
/// { "id": 42 }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSelector {
    /// Canonical socket-qualified address.
    Addr(PaneAddr),
    /// Legacy bare pane id; daemon resolves by scanning all sockets.
    Id(u64),
}

impl PaneSelector {
    /// The pane id regardless of selector form.
    pub fn id(&self) -> u64 {
        match self {
            Self::Addr(a) => a.id,
            Self::Id(id) => *id,
        }
    }

    /// Canonical address if known, else `None`.
    pub fn addr(&self) -> Option<&PaneAddr> {
        match self {
            Self::Addr(a) => Some(a),
            Self::Id(_) => None,
        }
    }

    /// True when the selector is the canonical address form.
    pub fn is_addr(&self) -> bool {
        matches!(self, Self::Addr(_))
    }
}

impl From<PaneAddr> for PaneSelector {
    fn from(addr: PaneAddr) -> Self {
        Self::Addr(addr)
    }
}

impl From<&PaneAddr> for PaneSelector {
    fn from(addr: &PaneAddr) -> Self {
        Self::Addr(addr.clone())
    }
}

impl From<u64> for PaneSelector {
    fn from(id: u64) -> Self {
        Self::Id(id)
    }
}

impl fmt::Display for PaneSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Addr(addr) => fmt::Display::fmt(addr, f),
            Self::Id(id) => write!(f, "{}", id),
        }
    }
}

/// Harness-native session id.
///
/// This preserves the provider's own stable id without assuming it is globally
/// unique across every harness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Globally unambiguous session key when multiple harnesses share id formats.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSessionKey {
    pub agent_kind: AgentKind,
    pub session_id: SessionId,
}

/// Where a pane activity observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    /// Native harness lifecycle hook. This is authoritative while fresh.
    Hook,
    /// Scrollback parser observation. This is recovery/poll evidence.
    Scrollback,
    /// Focus/read transition. This should affect read state, not invent work.
    Focus,
    /// Unknown source for compatibility or partial data.
    Unknown,
}

/// Canonical activity snapshot for a live pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneActivity {
    pub state: ActivityState,
    pub source: ActivitySource,
    pub observed_at_ms: i64,
    pub generation: u64,
}

impl PaneActivity {
    pub fn new(
        state: ActivityState,
        source: ActivitySource,
        observed_at_ms: i64,
        generation: u64,
    ) -> Self {
        Self {
            state,
            source,
            observed_at_ms,
            generation,
        }
    }

    pub fn next_generation(&self) -> u64 {
        self.generation.saturating_add(1)
    }
}

impl AgentSessionKey {
    pub fn new(agent_kind: AgentKind, session_id: impl Into<SessionId>) -> Self {
        Self {
            agent_kind,
            session_id: session_id.into(),
        }
    }

    pub fn storage_key(&self) -> String {
        self.agent_kind.session_key(self.session_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pane_addr_roundtrips_display_parse_and_json() {
        let addr = PaneAddr::new("unix:/run/user/1000/kitty.sock-12345", 42);

        assert_eq!(addr.short(), "42@12345");
        assert_eq!(addr.to_string(), "unix:/run/user/1000/kitty.sock-12345:42");
        assert_eq!(addr.to_string().parse::<PaneAddr>().unwrap(), addr);
        assert_eq!(
            serde_json::to_value(&addr).unwrap(),
            json!({
                "socket": "unix:/run/user/1000/kitty.sock-12345",
                "id": 42
            })
        );
    }

    #[test]
    fn pane_addr_distinguishes_same_id_on_different_sockets() {
        let one = PaneAddr::new("unix:/run/user/1000/kitty.sock-111", 7);
        let two = PaneAddr::new("unix:/run/user/1000/kitty.sock-222", 7);

        assert_ne!(one, two);
    }

    #[test]
    fn session_key_namespaces_provider_native_ids() {
        let claude = AgentSessionKey::new(AgentKind::Claude, "shared-id");
        let codex = AgentSessionKey::new(AgentKind::Codex, "shared-id");

        assert_eq!(claude.session_id.as_str(), "shared-id");
        assert_ne!(claude, codex);
        assert_eq!(claude.storage_key(), "claude:shared-id");
        assert_eq!(codex.storage_key(), "codex:shared-id");
    }

    #[test]
    fn session_id_is_transparent_json() {
        assert_eq!(
            serde_json::to_value(SessionId::new("sess-1")).unwrap(),
            json!("sess-1")
        );
    }

    #[test]
    fn pane_selector_addr_roundtrips_json() {
        let addr = PaneAddr::new("unix:/run/user/1000/kitty.sock-12345", 42);
        let selector = PaneSelector::from(addr.clone());

        let json = serde_json::to_value(&selector).unwrap();
        assert_eq!(
            json,
            json!({
                "addr": {
                    "socket": "unix:/run/user/1000/kitty.sock-12345",
                    "id": 42
                }
            })
        );
        let back: PaneSelector = serde_json::from_value(json).unwrap();
        assert_eq!(back, selector);
        assert_eq!(back.id(), 42);
        assert_eq!(back.addr(), Some(&addr));
        assert!(back.is_addr());
    }

    #[test]
    fn pane_selector_id_roundtrips_json() {
        let selector = PaneSelector::from(42u64);

        let json = serde_json::to_value(&selector).unwrap();
        assert_eq!(json, json!({ "id": 42 }));
        let back: PaneSelector = serde_json::from_value(json).unwrap();
        assert_eq!(back, selector);
        assert_eq!(back.id(), 42);
        assert_eq!(back.addr(), None);
        assert!(!back.is_addr());
    }

    #[test]
    fn pane_selector_id_and_addr_with_same_id_are_distinct() {
        let addr = PaneAddr::new("unix:/run/user/1000/kitty.sock-9", 7);
        assert_ne!(PaneSelector::from(addr), PaneSelector::from(7u64));
    }

    #[test]
    fn pane_activity_carries_source_time_and_generation() {
        let activity = PaneActivity::new(ActivityState::Thinking, ActivitySource::Hook, 1000, 7);

        assert_eq!(activity.next_generation(), 8);
        assert_eq!(
            serde_json::to_value(&activity).unwrap(),
            json!({
                "state": "thinking",
                "source": "hook",
                "observed_at_ms": 1000,
                "generation": 7
            })
        );
    }
}
