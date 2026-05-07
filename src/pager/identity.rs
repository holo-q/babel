use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use serde_json::{json, Value};

use crate::AgentKind;

use super::session_list::{EnrichedSession, RunningStatus};

const SQLITE_TEXT_CLIPBOARD_LIMIT: usize = 4096;

/// Debug identity payload for a native session row.
///
/// This is intentionally disk-shaped rather than display-shaped: `babel
/// resume` already shows the human summary, while `y` is for debugging the
/// exact native storage identity that produced the row.
pub fn session_identity_json(session: &EnrichedSession) -> Result<String> {
    let value = json!({
        "schema": "babel.resume.session_identity.v1",
        "captured_at_unix": now_secs(),
        "session": session_fields(session),
        "harness": harness_fields(session),
        "disk": disk_identity(session),
    });
    serde_json::to_string_pretty(&value).context("format session identity")
}

pub fn copy_to_clipboard(text: &str) -> Result<String> {
    let mut last_error = None;
    for target in clipboard_targets() {
        match copy_with_target(target, text) {
            Ok(()) => return Ok(target.label.to_string()),
            Err(error) if is_not_found(&error) => continue,
            Err(error) => last_error = Some(format!("{}: {error}", target.label)),
        }
    }
    if let Some(error) = last_error {
        anyhow::bail!("clipboard write failed: {error}");
    }
    anyhow::bail!("no clipboard command found: tried wl-copy, xclip, xsel, pbcopy")
}

fn session_fields(session: &EnrichedSession) -> Value {
    json!({
        "agent_kind": session.agent_kind.slug(),
        "agent_display": session.agent_kind.display_name(),
        "native_id": &session.native_id,
        "session_key": &session.session_key,
        "project_path": session.project_path.as_ref().map(|path| path.display().to_string()),
        "display_name": &session.display_name,
        "generated_title": &session.generated_title,
        "last_prompt": &session.last_prompt,
        "turn_count": session.turn_count,
        "created_at": session.created_at,
        "last_seen_at": session.last_seen_at,
        "interactive": session.interactive,
        "command_only": session.command_only,
        "has_title": session.has_title,
        "hidden": session.hidden,
        "custom_icon": session.custom_icon,
        "unread": session.unread,
        "running": running_fields(&session.running_status),
    })
}

fn harness_fields(session: &EnrichedSession) -> Value {
    let spec = session.agent_kind.spec();
    json!({
        "slug": spec.slug,
        "display": spec.display,
        "support": format!("{:?}", spec.support),
        "identity_fields": spec.identity_fields,
        "env_identity_fields": spec.env_identity_fields,
        "cmdline_markers": spec.cmdline_markers,
        "resume_command": spec.resume_command(&session.native_id),
    })
}

fn running_fields(status: &RunningStatus) -> Value {
    match status {
        RunningStatus::Inactive => json!({
            "active": false,
        }),
        RunningStatus::Active {
            pane_id,
            workspace,
            focused,
            hook_state,
            activity_state,
        } => json!({
            "active": true,
            "pane_id": pane_id,
            "workspace": workspace,
            "focused": focused,
            "hook_state": hook_state.map(|state| state.as_str()),
            "activity_state": format!("{activity_state:?}"),
        }),
    }
}

fn disk_identity(session: &EnrichedSession) -> Value {
    match session.agent_kind {
        AgentKind::Claude => claude_disk_identity(session),
        AgentKind::Codex => codex_disk_identity(session),
        AgentKind::Gemini => gemini_disk_identity(session),
        AgentKind::Amp => rooted_json_identity(
            session,
            "amp",
            &[
                home_path(&[".config", "amp"]),
                home_path(&[".local", "share", "amp"]),
            ],
        ),
        AgentKind::Kimi => rooted_json_identity(session, "kimi", &[home_path(&[".kimi"])]),
        AgentKind::OpenCode => rooted_json_identity(
            session,
            "opencode",
            &[
                xdg_data_path(&["opencode"]),
                home_path(&[".local", "share", "opencode"]),
            ],
        ),
        AgentKind::Crush => rooted_json_identity(
            session,
            "crush",
            &compact_paths(vec![
                env_path("CRUSH_DB"),
                env_path("CRUSH_DB_PATH"),
                env_path("CRUSH_DATA_DIR"),
                Some(xdg_data_path(&["crush"])),
                Some(home_path(&[".local", "share", "crush"])),
            ]),
        ),
        AgentKind::Cursor => rooted_json_identity(session, "cursor", &[home_path(&[".cursor"])]),
        AgentKind::Cline => cline_family_identity(
            session,
            "cline",
            &["CLINE_STORAGE_PATH", "CONTINUES_CLINE_STORAGE_PATH"],
            &["saoudrizwan.claude-dev"],
        ),
        AgentKind::RooCode => cline_family_identity(
            session,
            "roo-code",
            &[
                "ROO_CODE_STORAGE_PATH",
                "ROO_CLINE_STORAGE_PATH",
                "CONTINUES_ROO_CODE_STORAGE_PATH",
            ],
            &["rooveterinaryinc.roo-cline", "roo-code.roo-cline"],
        ),
        AgentKind::KiloCode => cline_family_identity(
            session,
            "kilo-code",
            &["KILO_CODE_STORAGE_PATH", "CONTINUES_KILO_CODE_STORAGE_PATH"],
            &["kilocode.kilo-code"],
        ),
        AgentKind::Kiro => rooted_json_identity(
            session,
            "kiro",
            &[
                home_path(&[
                    ".config",
                    "Kiro",
                    "User",
                    "globalStorage",
                    "kiro.kiroagent",
                    "workspace-sessions",
                ]),
                home_path(&[".kiro", "sessions", "cli"]),
            ],
        ),
        AgentKind::GithubCopilot => rooted_json_identity(
            session,
            "github-copilot-cli",
            &compact_paths(vec![
                env_path("COPILOT_HOME"),
                Some(home_path(&[".copilot", "session-state"])),
            ]),
        ),
        AgentKind::QwenCode => rooted_json_identity(
            session,
            "qwen-code",
            &compact_paths(vec![
                env_path("QWEN_RUNTIME_DIR"),
                env_path("QWEN_HOME"),
                Some(home_path(&[".qwen"])),
            ]),
        ),
        AgentKind::FactoryDroid => rooted_json_identity(
            session,
            "factory-droid",
            &[
                home_path(&[".factory", "projects"]),
                home_path(&[".factory", "sessions"]),
            ],
        ),
        AgentKind::Antigravity => rooted_json_identity(
            session,
            "antigravity",
            &compact_paths(vec![
                env_path("ANTIGRAVITY_HOME"),
                env_path("GEMINI_CLI_HOME").map(|path| path.join("antigravity")),
                Some(home_path(&[".gemini", "antigravity"])),
            ]),
        ),
        AgentKind::Aider => aider_disk_identity(session),
        AgentKind::Other => rooted_json_identity(session, "other", &[]),
    }
}

fn claude_disk_identity(session: &EnrichedSession) -> Value {
    let base = crate::utility::claude_storage::claude_base();
    let history = base.join("history.jsonl");
    let projects = base.join("projects");
    let project_dir = session
        .project_path
        .as_ref()
        .map(|cwd| projects.join(crate::utility::claude_storage::path_to_encoded(cwd)));
    let direct_session = project_dir
        .as_ref()
        .map(|dir| dir.join(format!("{}.jsonl", session.native_id)));
    let transcript = crate::utility::claude_storage::find_session_transcript(&session.native_id)
        .ok()
        .flatten();

    let mut roots = vec![base.clone(), projects.clone()];
    if let Some(project_dir) = &project_dir {
        roots.push(project_dir.clone());
    }

    json!({
        "kind": "claude",
        "roots": path_facts(&roots),
        "files": path_facts(&compact_paths(vec![
            Some(history),
            Some(base.join("settings.json")),
            Some(base.join("settings.local.json")),
            Some(base.join("todos")),
            Some(base.join("usage-data/session-meta")),
            direct_session,
            transcript,
        ])),
        "matching_files": path_facts(&find_matching_files(&[projects], &session.native_id, 8)),
    })
}

fn codex_disk_identity(session: &EnrichedSession) -> Value {
    let base = home_path(&[".codex"]);
    let state_db = find_codex_state_db(&base);
    let rollout = crate::harness::codex::transcript::find_session_transcript(&session.native_id)
        .ok()
        .flatten();
    let state_row = state_db
        .as_ref()
        .and_then(|path| read_codex_thread_row(path, &session.native_id).ok());
    let session_roots = vec![base.join("sessions"), base.join("archived_sessions")];

    json!({
        "kind": "codex",
        "roots": path_facts(&[
            base.clone(),
            base.join("sessions"),
            base.join("archived_sessions"),
            base.join("shell_snapshots"),
        ]),
        "files": path_facts(&compact_paths(vec![
            state_db,
            Some(base.join("history.jsonl")),
            Some(base.join("session_index.jsonl")),
            Some(base.join("config.toml")),
            Some(base.join("config.json")),
            Some(base.join("internal_storage.json")),
            rollout,
        ])),
        "state_db_thread_row": state_row,
        "matching_files": path_facts(&find_matching_files(&session_roots, &session.native_id, 8)),
    })
}

fn gemini_disk_identity(session: &EnrichedSession) -> Value {
    let base = home_path(&[".gemini"]);
    let roots = vec![base.join("tmp"), base.join("sessions")];

    json!({
        "kind": "gemini",
        "roots": path_facts(&[
            base.clone(),
            base.join("tmp"),
            base.join("sessions"),
            base.join("antigravity"),
        ]),
        "files": path_facts(&[
            base.join("projects.json"),
            base.join("settings.json"),
            base.join("history.jsonl"),
        ]),
        "matching_files": path_facts(&find_matching_files(&roots, &session.native_id, 8)),
    })
}

fn aider_disk_identity(session: &EnrichedSession) -> Value {
    let mut files = Vec::new();
    if let Some(project) = &session.project_path {
        files.push(project.join(".aider.chat.history.md"));
        files.push(project.join(".aider.input.history"));
        files.push(project.join(".aider.conf.yml"));
        files.push(project.join(".aider.conf.yaml"));
    }
    json!({
        "kind": "aider",
        "roots": path_facts(&compact_paths(vec![session.project_path.clone()])),
        "files": path_facts(&files),
        "matching_files": path_facts(&find_matching_files(&compact_paths(vec![session.project_path.clone()]), &session.native_id, 8)),
    })
}

fn cline_family_identity(
    session: &EnrichedSession,
    kind: &str,
    env_vars: &[&str],
    extension_ids: &[&str],
) -> Value {
    let mut roots: Vec<PathBuf> = env_vars.iter().filter_map(|name| env_path(name)).collect();
    for host in editor_global_storage_hosts() {
        for extension_id in extension_ids {
            roots.push(host.join(extension_id));
        }
    }
    rooted_json_identity(session, kind, &roots)
}

fn rooted_json_identity(session: &EnrichedSession, kind: &str, roots: &[PathBuf]) -> Value {
    let roots = compact_paths(roots.iter().cloned().map(Some).collect());
    json!({
        "kind": kind,
        "roots": path_facts(&roots),
        "matching_files": path_facts(&find_matching_files(&roots, &session.native_id, 8)),
    })
}

fn read_codex_thread_row(db_path: &Path, session_id: &str) -> Result<Value> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut stmt = conn.prepare("SELECT * FROM threads WHERE id = ?1 LIMIT 1")?;
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(ToString::to_string)
        .collect();

    let values = stmt.query_row([session_id], |row| {
        let mut map = BTreeMap::new();
        for (idx, name) in names.iter().enumerate() {
            map.insert(name.clone(), sqlite_value(row.get_ref(idx)?));
        }
        Ok(map)
    })?;

    Ok(json!(values))
}

fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => clipped_sqlite_text(value),
        ValueRef::Blob(value) => json!({ "blob_bytes": value.len() }),
    }
}

fn clipped_sqlite_text(value: &[u8]) -> Value {
    let text = String::from_utf8_lossy(value);
    if text.len() <= SQLITE_TEXT_CLIPBOARD_LIMIT {
        return json!(text.into_owned());
    }

    let mut end = SQLITE_TEXT_CLIPBOARD_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "text_prefix": &text[..end],
        "text_bytes": value.len(),
        "truncated": true,
    })
}

fn find_codex_state_db(codex_home: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.starts_with("state_") && name.ends_with(".sqlite") {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

fn find_matching_files(roots: &[PathBuf], native_id: &str, max_matches: usize) -> Vec<PathBuf> {
    if native_id.is_empty() || max_matches == 0 {
        return Vec::new();
    }

    let mut matches = Vec::new();
    let mut stack: Vec<PathBuf> = roots.iter().filter(|path| path.exists()).cloned().collect();
    let mut visited = 0usize;

    while let Some(dir) = stack.pop() {
        if visited > 20_000 || matches.len() >= max_matches {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.contains(native_id) {
                matches.push(path);
                if matches.len() >= max_matches {
                    break;
                }
            }
        }
    }

    matches.sort();
    matches
}

fn path_facts(paths: &[PathBuf]) -> Vec<Value> {
    paths.iter().map(|path| path_fact(path)).collect()
}

fn path_fact(path: &Path) -> Value {
    let metadata = std::fs::metadata(path).ok();
    let modified_at = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    json!({
        "path": path.display().to_string(),
        "exists": path.exists(),
        "is_file": path.is_file(),
        "is_dir": path.is_dir(),
        "bytes": metadata.as_ref().map(|metadata| metadata.len()),
        "modified_at_unix": modified_at,
        "canonical": std::fs::canonicalize(path).ok().map(|path| path.display().to_string()),
    })
}

fn compact_paths(paths: Vec<Option<PathBuf>>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = paths.into_iter().flatten().collect();
    paths.sort();
    paths.dedup();
    paths
}

fn editor_global_storage_hosts() -> Vec<PathBuf> {
    let mut hosts = Vec::new();
    for base in [home_path(&[".config"]), home_path(&[".local", "share"])] {
        for name in ["Code", "Code - Insiders", "VSCodium", "Cursor", "Windsurf"] {
            hosts.push(base.join(name).join("User").join("globalStorage"));
        }
    }
    hosts
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn xdg_data_path(parts: &[&str]) -> PathBuf {
    let mut path = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path(&[".local", "share"]));
    for part in parts {
        path.push(part);
    }
    path
}

fn home_path(parts: &[&str]) -> PathBuf {
    let mut path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    for part in parts {
        path.push(part);
    }
    path
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

struct ClipboardTarget {
    label: &'static str,
    command: &'static str,
    args: &'static [&'static str],
}

fn clipboard_targets() -> &'static [ClipboardTarget] {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[
            ClipboardTarget {
                label: "wl-copy",
                command: "wl-copy",
                args: &[],
            },
            ClipboardTarget {
                label: "xclip",
                command: "xclip",
                args: &["-selection", "clipboard"],
            },
            ClipboardTarget {
                label: "xsel",
                command: "xsel",
                args: &["--clipboard", "--input"],
            },
            ClipboardTarget {
                label: "pbcopy",
                command: "pbcopy",
                args: &[],
            },
        ]
    } else {
        &[
            ClipboardTarget {
                label: "xclip",
                command: "xclip",
                args: &["-selection", "clipboard"],
            },
            ClipboardTarget {
                label: "xsel",
                command: "xsel",
                args: &["--clipboard", "--input"],
            },
            ClipboardTarget {
                label: "pbcopy",
                command: "pbcopy",
                args: &[],
            },
        ]
    }
}

fn copy_with_target(target: &ClipboardTarget, text: &str) -> Result<()> {
    let mut child = Command::new(target.command)
        .args(target.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {}", target.label))?;

    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("open stdin for {}", target.label))?;
    stdin
        .write_all(text.as_bytes())
        .with_context(|| format!("write {}", target.label))?;
    drop(stdin);

    // Clipboard helpers either daemonize/linger as the selection owner or exit
    // quickly after handing the data to the platform. A helper that exits
    // immediately with failure must not be reported as a successful yank just
    // because stdin accepted bytes.
    thread::sleep(Duration::from_millis(25));
    if let Some(status) = child.try_wait()? {
        if status.success() {
            return Ok(());
        }
        anyhow::bail!("{} exited with {status}", target.label);
    }

    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|error| error.downcast_ref::<std::io::Error>())
        .any(|error| error.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::session_list::RunningStatus;

    #[test]
    fn identity_payload_contains_native_and_disk_identity() {
        let session = EnrichedSession {
            agent_kind: AgentKind::Codex,
            native_id: "abc123".to_string(),
            session_key: "codex:abc123".to_string(),
            project_path: Some(PathBuf::from("/workspace/project")),
            display_name: Some("thread".to_string()),
            generated_title: None,
            last_prompt: Some("prompt".to_string()),
            turn_count: 3,
            created_at: 10,
            last_seen_at: 20,
            interactive: true,
            command_only: false,
            has_title: true,
            hidden: false,
            custom_icon: None,
            unread: false,
            running_status: RunningStatus::Inactive,
        };

        let text = session_identity_json(&session).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["session"]["native_id"], "abc123");
        assert_eq!(value["session"]["session_key"], "codex:abc123");
        assert_eq!(value["disk"]["kind"], "codex");
    }
}
