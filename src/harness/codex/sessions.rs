use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::native_sessions::shared::{is_slash_command, truncate_str, NativeSession};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct CodexScanner;

impl NativeSessionScanner for CodexScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

/// Scan Codex CLI sessions from state_*.sqlite, enriched by history.jsonl.
fn scan() -> Result<Vec<NativeSession>> {
    let codex_home = dirs::home_dir().context("no home dir")?.join(".codex");
    if !codex_home.exists() {
        return Ok(Vec::new());
    }

    #[derive(serde::Deserialize)]
    struct ThreadEntry {
        id: String,
        thread_name: String,
    }
    let mut thread_names: HashMap<String, String> = HashMap::new();
    let idx_path = codex_home.join("session_index.jsonl");
    if idx_path.exists() {
        if let Ok(file) = std::fs::File::open(&idx_path) {
            for line in BufReader::new(file).lines().flatten() {
                if let Ok(e) = serde_json::from_str::<ThreadEntry>(&line) {
                    if !e.thread_name.is_empty() {
                        thread_names.insert(e.id, e.thread_name);
                    }
                }
            }
        }
    }

    struct PromptAcc {
        first_real: Option<String>,
        last_real: Option<String>,
        turns: u32,
        all_commands: bool,
    }
    let mut prompt_data: HashMap<String, PromptAcc> = HashMap::new();

    #[derive(serde::Deserialize)]
    struct HistEntry {
        session_id: String,
        text: String,
    }

    let hist_path = codex_home.join("history.jsonl");
    if hist_path.exists() {
        if let Ok(file) = std::fs::File::open(&hist_path) {
            for line in BufReader::new(file).lines().flatten() {
                if let Ok(e) = serde_json::from_str::<HistEntry>(&line) {
                    if e.session_id.is_empty() {
                        continue;
                    }
                    let is_cmd = is_slash_command(&e.text);
                    let real = if is_cmd {
                        None
                    } else {
                        Some(truncate_str(&e.text, 100))
                    };
                    prompt_data
                        .entry(e.session_id)
                        .and_modify(|acc| {
                            if !is_cmd {
                                acc.all_commands = false;
                                acc.last_real = real.clone();
                                if acc.first_real.is_none() {
                                    acc.first_real = real.clone();
                                }
                            }
                            acc.turns += 1;
                        })
                        .or_insert(PromptAcc {
                            first_real: real.clone(),
                            last_real: real,
                            turns: 1,
                            all_commands: is_cmd,
                        });
                }
            }
        }
    }

    if let Some(db_path) = find_state_db(&codex_home) {
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            let mut stmt = conn.prepare(
                "SELECT id, cwd, title, first_user_message, updated_at, has_user_event, agent_nickname
                 FROM threads
                 WHERE archived = 0
                 ORDER BY updated_at DESC",
            )?;

            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?;

            let mut out = Vec::new();
            for row in rows.flatten() {
                let (id, cwd, title, first_msg, updated_at, has_user_event, thread_name) = row;
                let prompts = prompt_data.remove(&id);
                let turn_count = prompts.as_ref().map(|p| p.turns).unwrap_or(0);
                let cmd_only = prompts.as_ref().map(|p| p.all_commands).unwrap_or(false);

                let thread_name = thread_names
                    .remove(&id)
                    .or(thread_name)
                    .filter(|t| !t.is_empty());
                let has_title =
                    thread_name.is_some() || title.as_ref().is_some_and(|t| !t.is_empty());
                let display_name = thread_name
                    .or(title.filter(|t| !t.is_empty()))
                    .or_else(|| first_msg.filter(|m| !m.is_empty() && !is_slash_command(m)))
                    .or_else(|| prompts.as_ref().and_then(|p| p.first_real.clone()));

                let last_prompt =
                    prompts.and_then(|p| if p.turns > 1 { p.last_real } else { None });

                out.push(NativeSession {
                    agent_kind: AgentKind::Codex,
                    native_id: id,
                    project_path: cwd,
                    display_name,
                    last_prompt,
                    turn_count,
                    last_seen_at: updated_at,
                    has_title,
                    interactive: has_user_event || turn_count > 0,
                    command_only: cmd_only,
                });
            }
            return Ok(out);
        }
    }

    Ok(prompt_data
        .into_iter()
        .map(|(sid, acc)| NativeSession {
            agent_kind: AgentKind::Codex,
            native_id: sid,
            project_path: None,
            display_name: acc.first_real,
            last_prompt: if acc.turns > 1 { acc.last_real } else { None },
            turn_count: acc.turns,
            last_seen_at: 0,
            interactive: true,
            command_only: acc.all_commands,
            has_title: false,
        })
        .collect())
}

fn find_state_db(codex_home: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            if name.starts_with("state_") && name.ends_with(".sqlite") {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.pop()
}
