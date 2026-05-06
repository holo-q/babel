//! Shared row-cell policy for session list surfaces.
//!
//! `babel ls-sessions` and `babel resume` need to telegraph the same facts in
//! the same order. Renderers own color and widget mechanics; this module owns
//! the semantic cells so the CLI table and TUI browser cannot drift quietly.

use std::path::Path;

use crate::agent_kind::AgentKind;
use crate::babel_storage::HookState;
use crate::ActivityState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    Idle,
    Working,
    ToolRunning,
    Thinking,
    PlanApproval,
    AwaitingInput,
    BackgroundTask,
    Unknown,
    NotRunning,
}

#[derive(Clone, Copy, Debug)]
pub struct LiveSessionState {
    pub workspace: Option<i32>,
    pub hook_state: Option<HookState>,
    pub activity_state: Option<ActivityState>,
}

#[derive(Clone, Copy)]
pub struct SessionRowInput<'a> {
    pub agent_kind: AgentKind,
    pub native_id: &'a str,
    pub project_path: Option<&'a Path>,
    pub display_name: Option<&'a str>,
    pub generated_title: Option<&'a str>,
    pub last_prompt: Option<&'a str>,
    pub turn_count: u32,
    pub last_seen_at: i64,
    pub interactive: bool,
    pub command_only: bool,
    pub has_title: bool,
    pub hidden: bool,
    pub live: Option<LiveSessionState>,
    pub text_max_chars: usize,
}

/// Precomputed display cells for one session row.
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub state_icon: &'static str,
    pub state_kind: StateKind,
    pub harness: String,
    /// Filter category sigil: ◇ oneshot, / command-only, ⊞ subagent, ▪ hidden.
    pub filter_tag: &'static str,
    pub workspace: String,
    pub cwd: String,
    pub time: String,
    pub turns: String,
    pub title: String,
    pub last_prompt: String,
    pub accent: &'static str,
    pub bright: bool,
    pub has_title: bool,
}

impl SessionRow {
    pub fn is_running(&self) -> bool {
        !matches!(self.state_kind, StateKind::NotRunning)
    }
}

pub fn session_row(input: SessionRowInput<'_>, now: i64) -> SessionRow {
    let (state_icon, state_kind) = live_state_icon(input.live);

    let cwd = input
        .project_path
        .map(|path| abbreviate_path(path, usize::MAX))
        .unwrap_or_default();

    let (title, has_title) = if let Some(generated) = input.generated_title {
        (sanitize_display(generated, input.text_max_chars), true)
    } else {
        let raw = input.display_name.unwrap_or(input.native_id);
        (sanitize_display(raw, input.text_max_chars), input.has_title)
    };

    let last_prompt = input
        .last_prompt
        .map(|text| sanitize_display(text, input.text_max_chars))
        .unwrap_or_default();

    let turns = if input.turn_count > 0 {
        format!("{}t", input.turn_count)
    } else {
        String::new()
    };

    let workspace = input
        .live
        .and_then(|live| live.workspace)
        .map(|ws| format!("{}", ws + 1))
        .unwrap_or_default();

    let elapsed = now - input.last_seen_at;
    let time = relative_time(elapsed);

    let filter_tag = if input.hidden {
        "▪"
    } else if !input.interactive {
        "⊞"
    } else if input.command_only {
        "/"
    } else if input.turn_count == 1 {
        "◇"
    } else {
        " "
    };

    // `0t` means this harness scanner does not know turn counts yet. Only an
    // explicit single turn is treated as low-signal/oneshot and dimmed.
    let bright = input.interactive && !input.hidden && !input.command_only && input.turn_count != 1;

    SessionRow {
        state_icon,
        state_kind,
        harness: input.agent_kind.slug().to_string(),
        filter_tag,
        workspace,
        cwd,
        time,
        turns,
        title,
        last_prompt,
        accent: input.agent_kind.accent_color(),
        bright,
        has_title,
    }
}

pub fn live_state_icon(live: Option<LiveSessionState>) -> (&'static str, StateKind) {
    let Some(live) = live else {
        return (" ", StateKind::NotRunning);
    };

    match (live.hook_state, live.activity_state) {
        (Some(HookState::Idle), _) => ("○", StateKind::Idle),
        (Some(HookState::ToolRunning), _) => ("⚙", StateKind::ToolRunning),
        (Some(HookState::Working), Some(ActivityState::Thinking)) => ("⚡", StateKind::Thinking),
        (Some(HookState::Working), Some(ActivityState::ToolUse)) => ("⚙", StateKind::ToolRunning),
        (Some(HookState::Working), Some(ActivityState::PlanApproval)) => {
            ("📋", StateKind::PlanApproval)
        }
        (Some(HookState::Working), Some(ActivityState::BackgroundTask)) => {
            ("◐", StateKind::BackgroundTask)
        }
        (Some(HookState::Working), _) => ("●", StateKind::Working),
        (None, Some(ActivityState::Thinking)) => ("⚡", StateKind::Thinking),
        (None, Some(ActivityState::ToolUse)) => ("⚙", StateKind::ToolRunning),
        (None, Some(ActivityState::PlanApproval)) => ("📋", StateKind::PlanApproval),
        (None, Some(ActivityState::AwaitingInput)) => ("◆", StateKind::AwaitingInput),
        (None, Some(ActivityState::BackgroundTask)) => ("◐", StateKind::BackgroundTask),
        (None, Some(ActivityState::Idle)) => ("○", StateKind::Idle),
        (None, Some(ActivityState::Unknown)) | (None, None) => ("◌", StateKind::Unknown),
    }
}

pub fn relative_time(seconds_ago: i64) -> String {
    if seconds_ago < 0 {
        return "now".to_string();
    }
    if seconds_ago < 60 {
        return format!("{}s", seconds_ago);
    }
    let minutes = seconds_ago / 60;
    if minutes < 60 {
        return format!("{}m", minutes);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{}h", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d", days);
    }
    let months = days / 30;
    format!("{}mo", months)
}

pub fn abbreviate_path(path: &Path, max_chars: usize) -> String {
    let path_str = path.to_string_lossy();

    let mut out = if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path_str.starts_with(home_str.as_ref()) {
            format!("~{}", &path_str[home_str.len()..])
        } else {
            path_str.to_string()
        }
    } else {
        path_str.to_string()
    };

    if max_chars != usize::MAX {
        out = truncate_chars(&out, max_chars);
    }
    out
}

pub fn sanitize_display(s: &str, max_chars: usize) -> String {
    let clean = s.split_whitespace().collect::<Vec<_>>().join(" ");
    middle_truncate_chars(&clean, max_chars)
}

pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let short: String = s.chars().take(max_chars).collect();
        format!("{}…", short)
    }
}

pub fn middle_truncate_chars(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }

    const MARKER: &str = "… [cut] …";
    let marker_len = MARKER.chars().count();
    if max_chars <= marker_len + 2 {
        return truncate_chars(s, max_chars);
    }

    let keep = max_chars - marker_len;
    let head_len = keep / 2;
    let tail_len = keep.saturating_sub(head_len);
    let head: String = s.chars().take(head_len).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}{MARKER}{tail}")
}

fn hex_to_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

pub fn closest_ansi256_from_hex(hex: &str) -> u8 {
    let (r, g, b) = hex_to_rgb(hex).unwrap_or((102, 102, 102));
    let ri = ((r as u16) * 5 / 255) as u8;
    let gi = ((g as u16) * 5 / 255) as u8;
    let bi = ((b as u16) * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}
