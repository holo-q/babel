//! IPC Protocol - The voice channel through the tower
//!
//! This module implements the communication protocol between Babel's many workers
//! and the world outside. Requests descend into the tower, responses ascend bearing
//! knowledge from the collective.
//!
//! **Protocol**: Newline-delimited JSON over Unix socket
//! **Channel**: `$XDG_RUNTIME_DIR/babel.sock` (or `/tmp/babel.sock` fallback)
//! **Pattern**: Request/Response - queries flow down, answers flow up
//!
//! The socket is where voices meet: CLI tools speak to the daemon's receptive workers,
//! and in the future, a Captain will use this same channel to orchestrate the anima below.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::instrument;

// Wire DTOs live in `crate::ipc`. They are re-exported here so callers of
// `babel::utility::ipc::{Request, Response, TitleTarget}` keep compiling while
// the canonical home becomes the top-level `ipc` module. New code should
// import DTOs from `crate::ipc` and only reach into `utility::ipc` for
// transport (socket_path, create_listener, send_request, ...).
pub use crate::ipc::{Request, Response, TitleTarget};

// ═══════════════════════════════════════════════════════════════════════════════
// Socket Path
// ═══════════════════════════════════════════════════════════════════════════════

/// Get the daemon socket path - where voices meet
///
/// This is the rendezvous point where external queries enter the tower and
/// responses emerge. The Captain, when implemented, will use this same channel
/// to direct the workers below.
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("babel.sock")
    } else {
        PathBuf::from("/tmp").join(format!("babel-{}.sock", users::get_current_uid()))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Client
// ═══════════════════════════════════════════════════════════════════════════════

/// Send a request to the daemon and get a response
///
/// The fundamental dialogue: speak a query into the tower, await the answer that rises.
#[tracing::instrument(level = "debug", skip(request), fields(cmd = ?request))]
pub async fn send_request(request: &Request) -> Result<Response> {
    let sock_path = socket_path();

    let mut stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("Failed to connect to daemon at {}", sock_path.display()))?;

    // Send request as JSON line
    let mut request_json = serde_json::to_string(request)?;
    request_json.push('\n');
    stream.write_all(request_json.as_bytes()).await?;

    // Read response
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;

    let response: Response =
        serde_json::from_str(&response_line).context("Failed to parse daemon response")?;

    Ok(response)
}

/// Check if daemon is running
///
/// Fast check: just verifies socket exists and is connectable.
/// Doesn't send a message - saves one IPC round-trip (~5ms).
#[instrument(level = "debug")]
pub async fn is_daemon_running() -> bool {
    let sock_path = socket_path();
    // Quick check: can we connect to the socket?
    // This is faster than sending a ping and waiting for response
    UnixStream::connect(&sock_path).await.is_ok()
}

/// Synchronous wrapper for send_request (for non-async CLI)
#[instrument(level = "debug", skip(request), fields(cmd = ?request))]
pub fn send_request_sync(request: &Request) -> Result<Response> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(send_request(request))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Server
// ═══════════════════════════════════════════════════════════════════════════════

/// Create and bind the daemon socket
///
/// Establishes the listening point where the tower receives its queries.
/// This is the daemon opening itself to communication from above.
#[instrument(level = "debug")]
pub async fn create_listener() -> Result<UnixListener> {
    let sock_path = socket_path();

    // Remove existing socket if present
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)
            .with_context(|| format!("Failed to remove old socket at {}", sock_path.display()))?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("Failed to bind socket at {}", sock_path.display()))?;

    // Make socket accessible
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(listener)
}

/// Read a request from a client connection
///
/// Listen for a query arriving at the tower's threshold.
#[instrument(level = "debug", skip(stream))]
pub async fn read_request(stream: &mut BufReader<UnixStream>) -> Result<Option<Request>> {
    let mut line = String::new();
    let bytes_read = stream.read_line(&mut line).await?;

    if bytes_read == 0 {
        return Ok(None); // Connection closed
    }

    let request: Request = serde_json::from_str(&line).context("Failed to parse client request")?;

    Ok(Some(request))
}

/// Send a response to a client
///
/// Return knowledge upward through the channel - the tower speaks its answer.
#[instrument(level = "debug", skip(stream, response))]
pub async fn send_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let mut response_json = serde_json::to_string(response)?;
    response_json.push('\n');
    stream.write_all(response_json.as_bytes()).await?;
    Ok(())
}
