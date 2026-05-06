use crate::agent_kind::{HookEventSpec, HookStateEffect, PulseEffect, ReadEffect};

pub(crate) const CLAUDE_CODE: &[HookEventSpec] = &[
    HookEventSpec {
        native: "SessionStart",
        canonical: "session-start",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Session,
    },
    HookEventSpec {
        native: "UserPromptSubmit",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "PreToolUse",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "PostToolUse",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "Stop",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "Notification",
        canonical: "notification",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Attention,
    },
    HookEventSpec {
        native: "SubagentStop",
        canonical: "subagent-stop",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "PreCompact",
        canonical: "pre-compact",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Compact,
    },
];

pub(crate) const CODEX: &[HookEventSpec] = &[
    HookEventSpec {
        native: "SessionStart",
        canonical: "session-start",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Session,
    },
    HookEventSpec {
        native: "UserPromptSubmit",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "PreToolUse",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "PostToolUse",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "Stop",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "PermissionRequest",
        canonical: "notification",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Attention,
    },
];

pub(crate) const GEMINI: &[HookEventSpec] = &[
    HookEventSpec {
        native: "BeforeTool",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "AfterTool",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "BeforeAgent",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "Stop",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "PreCompress",
        canonical: "pre-compact",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Compact,
    },
];

pub(crate) const CRUSH: &[HookEventSpec] = &[HookEventSpec {
    native: "PreToolUse",
    canonical: "pre-tool",
    state: Some(HookStateEffect::ToolRunning),
    read: ReadEffect::Preserve,
    pulse: PulseEffect::Tool,
}];

pub(crate) const CLINE: &[HookEventSpec] = &[
    HookEventSpec {
        native: "TaskStart",
        canonical: "session-start",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Session,
    },
    HookEventSpec {
        native: "UserPromptSubmit",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "PreToolUse",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "PostToolUse",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "TaskComplete",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
];

pub(crate) const NONE: &[HookEventSpec] = &[];
