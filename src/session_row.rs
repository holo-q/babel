//! Shared row-cell policy for session list surfaces.
//!
//! `babel ls-sessions` and `babel resume` need to telegraph the same facts in
//! the same order. Renderers own color and widget mechanics; this module owns
//! the semantic cells so the CLI table and TUI browser cannot drift quietly.

use std::path::Path;
use std::sync::OnceLock;

use crate::agent_kind::AgentKind;
use crate::babel_storage::HookState;
use crate::ActivityState;

fn cached_home_dir() -> Option<&'static str> {
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    HOME.get_or_init(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))
        .as_deref()
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgeTone {
    Fresh,
    Recent,
    Today,
    Week,
    Month,
    Old,
    Unknown,
}

const ANSI_RED: (u8, u8, u8) = (170, 0, 0);
const ANSI_GREEN: (u8, u8, u8) = (0, 170, 0);
const ANSI_YELLOW: (u8, u8, u8) = (170, 170, 0);
const ANSI_BRIGHT_BLACK: (u8, u8, u8) = (128, 128, 128);

impl AgeTone {
    pub fn from_elapsed_seconds(seconds_ago: i64) -> Self {
        if seconds_ago < 0 {
            return Self::Fresh;
        }

        const FIVE_MINUTES: i64 = 5 * 60;
        const ONE_HOUR: i64 = 60 * 60;
        const ONE_DAY: i64 = 24 * ONE_HOUR;
        const ONE_WEEK: i64 = 7 * ONE_DAY;
        const ONE_MONTH: i64 = 30 * ONE_DAY;

        if seconds_ago < FIVE_MINUTES {
            Self::Fresh
        } else if seconds_ago < ONE_HOUR {
            Self::Recent
        } else if seconds_ago < ONE_DAY {
            Self::Today
        } else if seconds_ago < ONE_WEEK {
            Self::Week
        } else if seconds_ago < ONE_MONTH {
            Self::Month
        } else {
            Self::Old
        }
    }

    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Fresh => mute(ANSI_GREEN, 70),
            Self::Recent => mute(lerp_rgb(ANSI_GREEN, ANSI_YELLOW, 25), 62),
            Self::Today => mute(ANSI_YELLOW, 58),
            Self::Week => mute(lerp_rgb(ANSI_YELLOW, ANSI_RED, 50), 56),
            Self::Month => mute(ANSI_RED, 54),
            Self::Old | Self::Unknown => ANSI_BRIGHT_BLACK,
        }
    }

    pub fn is_bold(self) -> bool {
        matches!(self, Self::Fresh)
    }

    pub fn should_dim(self, row_bright: bool) -> bool {
        !row_bright || matches!(self, Self::Old | Self::Unknown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnTone {
    count: u32,
}

impl TurnTone {
    pub const MAX_GRADIENT_TURNS: u32 = 150;

    pub fn from_turn_count(count: u32) -> Self {
        Self { count }
    }

    pub fn rgb(self) -> (u8, u8, u8) {
        if self.count == 0 {
            return ANSI_BRIGHT_BLACK;
        }

        let pct =
            ((self.count.min(Self::MAX_GRADIENT_TURNS) * 100) / Self::MAX_GRADIENT_TURNS) as u8;
        let hot = if pct <= 50 {
            lerp_rgb(ANSI_GREEN, ANSI_YELLOW, pct * 2)
        } else {
            lerp_rgb(ANSI_YELLOW, ANSI_RED, (pct - 50) * 2)
        };
        mute(hot, 52 + pct / 4)
    }

    pub fn is_bold(self) -> bool {
        self.count >= Self::MAX_GRADIENT_TURNS
    }

    pub fn should_dim(self, row_bright: bool) -> bool {
        !row_bright || self.count == 0
    }
}

fn lerp_rgb(from: (u8, u8, u8), to: (u8, u8, u8), to_percent: u8) -> (u8, u8, u8) {
    (
        lerp_channel(from.0, to.0, to_percent),
        lerp_channel(from.1, to.1, to_percent),
        lerp_channel(from.2, to.2, to_percent),
    )
}

fn lerp_channel(from: u8, to: u8, to_percent: u8) -> u8 {
    let from = from as i16;
    let to = to as i16;
    (from + ((to - from) * to_percent as i16) / 100) as u8
}

fn mute(rgb: (u8, u8, u8), color_percent: u8) -> (u8, u8, u8) {
    lerp_rgb(ANSI_BRIGHT_BLACK, rgb, color_percent)
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
    pub created_at: i64,
    pub last_seen_at: i64,
    pub interactive: bool,
    pub command_only: bool,
    pub has_title: bool,
    pub hidden: bool,
    pub live: Option<LiveSessionState>,
    pub text_max_chars: usize,
    /// When true, title/last_prompt are already sanitized — skip
    /// split_whitespace+join+truncate.
    pub pre_sanitized: bool,
}

/// Precomputed display cells for one session row.
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub state_icon: &'static str,
    pub state_kind: StateKind,
    pub harness: String,
    pub native_id: String,
    /// Filter category sigil: ◇ oneshot, / command-only, ⊞ subagent, ▪ hidden.
    pub filter_tag: &'static str,
    pub workspace: String,
    pub cwd: String,
    pub created_time: String,
    pub created_time_tone: AgeTone,
    pub modified_time: String,
    pub modified_time_tone: AgeTone,
    pub time: String,
    pub time_tone: AgeTone,
    pub turn_count: u32,
    pub turn_tone: TurnTone,
    pub turns: String,
    pub title: String,
    pub last_prompt: String,
    pub accent: &'static str,
    pub ansi256: u8,
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
        let text = if input.pre_sanitized {
            generated.to_string()
        } else {
            sanitize_display(generated, input.text_max_chars)
        };
        (text, true)
    } else {
        let raw = input.display_name.unwrap_or(input.native_id);
        let text = if input.pre_sanitized {
            raw.to_string()
        } else {
            sanitize_display(raw, input.text_max_chars)
        };
        (text, input.has_title)
    };

    let last_prompt = input
        .last_prompt
        .map(|text| {
            if input.pre_sanitized {
                text.to_string()
            } else {
                sanitize_display(text, input.text_max_chars)
            }
        })
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

    let created_elapsed = now - input.created_at;
    let modified_elapsed = now - input.last_seen_at;
    let created_time = relative_time(created_elapsed);
    let created_time_tone = AgeTone::from_elapsed_seconds(created_elapsed);
    let modified_time = relative_time(modified_elapsed);
    let modified_time_tone = AgeTone::from_elapsed_seconds(modified_elapsed);

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
        native_id: input.native_id.to_string(),
        filter_tag,
        workspace,
        cwd,
        created_time: created_time.clone(),
        created_time_tone,
        modified_time: modified_time.clone(),
        modified_time_tone,
        time: modified_time,
        time_tone: modified_time_tone,
        turn_count: input.turn_count,
        turn_tone: TurnTone::from_turn_count(input.turn_count),
        turns,
        title,
        last_prompt,
        accent: input.agent_kind.accent_color(),
        ansi256: input.agent_kind.accent_ansi256(),
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
        (Some(HookState::Working), Some(ActivityState::Thinking)) => ("↯", StateKind::Thinking),
        (Some(HookState::Working), Some(ActivityState::ToolUse)) => ("⚙", StateKind::ToolRunning),
        (Some(HookState::Working), Some(ActivityState::PlanApproval)) => {
            ("▣", StateKind::PlanApproval)
        }
        (Some(HookState::Working), Some(ActivityState::BackgroundTask)) => {
            ("◐", StateKind::BackgroundTask)
        }
        (Some(HookState::Working), _) => ("●", StateKind::Working),
        (None, Some(ActivityState::Thinking)) => ("↯", StateKind::Thinking),
        (None, Some(ActivityState::ToolUse)) => ("⚙", StateKind::ToolRunning),
        (None, Some(ActivityState::PlanApproval)) => ("▣", StateKind::PlanApproval),
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

    let mut out = if let Some(home) = cached_home_dir() {
        if path_str.starts_with(home) {
            format!("~{}", &path_str[home.len()..])
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

    const MARKER: &str = " ⌿ ";
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

#[cfg(test)]
mod tests {
    use super::TurnTone;

    #[test]
    fn turn_tone_caps_gradient_at_150_turns() {
        let max = TurnTone::from_turn_count(TurnTone::MAX_GRADIENT_TURNS);
        let over = TurnTone::from_turn_count(999);
        let lower = TurnTone::from_turn_count(75);

        assert_eq!(max.rgb(), over.rgb());
        assert_ne!(lower.rgb(), max.rgb());
        assert!(max.is_bold());
    }
}
