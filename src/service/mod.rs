//! Domain services.
//!
//! Services own state transitions and return semantic results. The daemon
//! should eventually become runtime orchestration around these services:
//! sockets, tasks, watchers, IPC, and effect execution.

pub mod activity;
pub mod refresh;
