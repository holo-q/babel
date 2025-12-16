//! TUI Debug Console - Interactive IPC traffic inspector
//!
//! A ratatui-based debug console for:
//! - Inspecting daemon state (windows, fired tasks)
//! - Watching IPC traffic (SEND/RECV/EVNT)
//! - Reference implementation for richmon and OS integrations
//!
//! Layout (65/35 split):
//! ```text
//! ┌────────────────────────────────────────────────────────────────────────────┐
//! │ babel tui                                   [daemon: ●] uptime: 2h 14m     │
//! ├───────────────────────┬─────────────────────┬──────────────────────────────┤
//! │ Windows          [F1] │ Fired Tasks    [F2] │ Details               [F3]  │
//! │ (25%)                 │ (25%)               │ (50%)                       │
//! ├───────────────────────┴─────────────────────┴──────────────────────────────┤
//! │ IPC Log (35%)                                                         [F4] │
//! └────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Keybinds:
//! - q/Esc: Quit
//! - Tab: Cycle panes
//! - F1-F4: Jump to pane
//! - j/k, ↑/↓: Navigate lists
//! - Enter: Select item → show in Details
//! - r: Force refresh
//! - a: Toggle auto-scroll (IPC log)
//! - c: Clear IPC log
//! - ?: Show help

mod app;
mod ipc_client;
mod ui;

pub use app::run_tui;
