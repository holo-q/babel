//! Kitty terminal integration -- re-export shim
//!
//! All implementation has moved to `backend/kitty.rs`. This module exists only
//! for backward compatibility during the migration to the multi-backend architecture.
//! Consumers should update their imports to use `crate::backend::kitty` directly.

// Re-export all kitty-specific types and functions
pub use crate::backend::kitty::*;

// Re-export shared types that consumers import via crate::kitty::
pub use crate::model::{PaneAddr, PaneSelector};

// Re-export desktop ops (moved to desktop.rs, previously lived here)
pub use crate::desktop::{
    get_all_workspaces, get_window_geometry, get_workspace, move_window_to_workspace,
    set_window_geometry,
};

// Re-export backend types that consumers expect from crate::kitty::
pub use crate::backend::{ForegroundProcess, ScreenGeometry};
