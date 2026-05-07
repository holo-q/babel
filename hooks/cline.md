# Cline — Babel Hook Wiring

Cline uses a filesystem-based hook configuration. Register these commands
in your Cline hook settings for each event:

| Cline Event         | Canonical | Command                                           |
|---------------------|-----------|---------------------------------------------------|
| TaskStart           | session-start | `babel hook stdin session-start --agent cline` |
| UserPromptSubmit    | prompt        | `babel hook stdin prompt --agent cline`        |
| PreToolUse          | pre-tool      | `babel hook stdin pre-tool --agent cline`      |
| PostToolUse         | post-tool     | `babel hook stdin post-tool --agent cline`     |
| TaskComplete        | stop          | `babel hook stdin stop --agent cline`          |
| Notification        | notification  | `babel hook stdin notification --agent cline`  |

Each hook receives the event payload on stdin and should be configured
as a shell command hook in your Cline extension settings.
