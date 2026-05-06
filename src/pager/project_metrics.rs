//! Cached transcript metrics for the resume pager.
//!
//! The session list is a launcher surface: cursor movement must stay hot even
//! when a transcript contains thousands of tool calls. Project touch history is
//! therefore a derived metric stored in Babel's database, keyed by the transcript
//! source mtime observed by the background parser.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::agent_kind::AgentKind;
use crate::babel_storage::{BabelStorage, SessionProjectCacheEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTouchMetric {
    pub path: PathBuf,
    pub touch_count: u32,
}

pub fn load_cached_session_projects(
    agent_kind: AgentKind,
    native_id: &str,
    session_key: &str,
) -> Result<Vec<ProjectTouchMetric>> {
    let Some(path) = find_session_transcript(agent_kind, native_id)? else {
        anyhow::bail!("transcript not found");
    };

    let source_mtime_ns = source_mtime_ns(&path)?;
    let source_path = path.display().to_string();
    let db = BabelStorage::open()?;

    if let Some(projects) =
        db.get_session_project_cache(session_key, &source_path, source_mtime_ns)?
    {
        return Ok(projects
            .into_iter()
            .map(|project| ProjectTouchMetric {
                path: PathBuf::from(project.path),
                touch_count: project.touch_count,
            })
            .collect());
    }

    let projects = parse_touched_projects(&path)?;
    let project_cache: Vec<SessionProjectCacheEntry> = projects
        .iter()
        .map(|project| SessionProjectCacheEntry {
            path: project.path.display().to_string(),
            touch_count: project.touch_count,
        })
        .collect();
    db.set_session_project_cache(session_key, &source_path, source_mtime_ns, &project_cache)?;

    Ok(projects)
}

pub fn find_session_transcript(agent_kind: AgentKind, native_id: &str) -> Result<Option<PathBuf>> {
    match agent_kind {
        AgentKind::Claude => crate::utility::claude_storage::find_session_transcript(native_id),
        AgentKind::Codex => crate::harness::codex::transcript::find_session_transcript(native_id),
        _ => Ok(None),
    }
}

pub fn parse_touched_projects(path: &Path) -> Result<Vec<ProjectTouchMetric>> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open transcript {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut metrics_by_project: HashMap<PathBuf, ProjectAccumulator> = HashMap::new();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let base_cwd = record_base_cwd(&record);
        let mut candidates = Vec::new();
        collect_path_candidates(&record, base_cwd.as_deref(), &mut candidates);

        for candidate in candidates {
            let project = project_root_for_path(&candidate);
            let entry = metrics_by_project.entry(project).or_default();
            entry.latest_line = entry.latest_line.max(line_index);
            entry.touch_count = entry.touch_count.saturating_add(1);
        }
    }

    let mut projects: Vec<(PathBuf, ProjectAccumulator)> = metrics_by_project.into_iter().collect();
    projects.sort_by(|(left_path, left), (right_path, right)| {
        right
            .latest_line
            .cmp(&left.latest_line)
            .then_with(|| left_path.cmp(right_path))
    });

    Ok(projects
        .into_iter()
        .map(|(path, metric)| ProjectTouchMetric {
            path,
            touch_count: metric.touch_count,
        })
        .collect())
}

#[derive(Debug, Clone, Copy, Default)]
struct ProjectAccumulator {
    latest_line: usize,
    touch_count: u32,
}

fn source_mtime_ns(path: &Path) -> Result<i64> {
    let modified = std::fs::metadata(path)?
        .modified()
        .context("transcript file has no modified timestamp")?;
    let duration = modified
        .duration_since(UNIX_EPOCH)
        .context("transcript modified timestamp predates unix epoch")?;
    Ok(duration.as_secs() as i64 * 1_000_000_000 + duration.subsec_nanos() as i64)
}

fn record_base_cwd(record: &Value) -> Option<PathBuf> {
    first_string_for_keys(record, &["cwd", "workdir"]).and_then(|cwd| candidate_path(cwd, None))
}

fn first_string_for_keys<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(text) = map.get(*key).and_then(Value::as_str) {
                    return Some(text);
                }
            }
            for child in map.values() {
                if let Some(text) = first_string_for_keys(child, keys) {
                    return Some(text);
                }
            }
            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| first_string_for_keys(child, keys)),
        _ => None,
    }
}

fn collect_path_candidates(value: &Value, base: Option<&Path>, out: &mut Vec<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "arguments" {
                    if let Some(parsed) = child
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    {
                        collect_path_candidates(&parsed, base, out);
                    }
                }

                if is_path_key(key) {
                    collect_path_value(child, base, out);
                }

                collect_path_candidates(child, base, out);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_path_candidates(child, base, out);
            }
        }
        _ => {}
    }
}

fn collect_path_value(value: &Value, base: Option<&Path>, out: &mut Vec<PathBuf>) {
    match value {
        Value::String(text) => {
            if let Some(path) = candidate_path(text, base) {
                out.push(path);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_path_value(value, base, out);
            }
        }
        _ => {}
    }
}

fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "cwd"
            | "workdir"
            | "file_path"
            | "path"
            | "notebook_path"
            | "rootPath"
            | "workspace"
            | "workspace_path"
            | "cwdOnTaskInitialization"
    )
}

fn candidate_path(raw: &str, base: Option<&Path>) -> Option<PathBuf> {
    let text = raw.trim();
    if text.is_empty()
        || text.starts_with("http://")
        || text.starts_with("https://")
        || text.starts_with("file://")
        || text.contains('\n')
        || text.contains('*')
        || text.contains('?')
    {
        return None;
    }

    let path = if let Some(rest) = text.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        PathBuf::from(text)
    };

    if path.is_absolute() {
        return Some(normalize_lexical_path(&path));
    }

    base.map(|base| normalize_lexical_path(&base.join(path)))
}

fn project_root_for_path(path: &Path) -> PathBuf {
    let normalized = normalize_lexical_path(path);
    let mut candidate =
        if normalized.is_file() || (!normalized.exists() && normalized.extension().is_some()) {
            normalized.parent().unwrap_or(&normalized).to_path_buf()
        } else {
            normalized
        };

    candidate = normalize_lexical_path(&candidate);
    for ancestor in candidate.ancestors() {
        if is_project_root(ancestor) {
            return ancestor.to_path_buf();
        }
    }

    candidate
}

fn is_project_root(path: &Path) -> bool {
    const MARKERS: &[&str] = &[
        ".git",
        "pyproject.toml",
        "Cargo.toml",
        "package.json",
        "pnpm-workspace.yaml",
        "yarn.lock",
        "deno.json",
        "go.mod",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "pom.xml",
        "mix.exs",
        "gleam.toml",
        "Package.swift",
        "*.sln",
    ];

    MARKERS.iter().any(|marker| {
        if *marker == "*.sln" {
            return std::fs::read_dir(path)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "sln"));
        }
        path.join(marker).exists()
    })
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn extracts_latest_touched_projects_from_tool_inputs() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-{}", std::process::id()));
        let alpha = root.join("alpha");
        let beta = root.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::write(alpha.join("pyproject.toml"), "").unwrap();
        std::fs::create_dir_all(alpha.join("src")).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        std::fs::write(beta.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(beta.join("lib")).unwrap();
        let transcript = root.join("session.jsonl");
        let mut file = File::create(&transcript).unwrap();

        writeln!(
            file,
            r#"{{"type":"assistant","cwd":"{}","message":{{"content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"src/lib.rs"}}}}]}}}}"#,
            alpha.display()
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":"{{\"cmd\":\"rg hi\",\"workdir\":\"{}\"}}"}}}}"#,
            beta.display()
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":"{}/src/main.rs"}}}}]}}}}"#,
            alpha.display()
        )
        .unwrap();
        drop(file);

        let projects = parse_touched_projects(&transcript).unwrap();
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(
            projects,
            vec![
                ProjectTouchMetric {
                    path: alpha,
                    touch_count: 3
                },
                ProjectTouchMetric {
                    path: beta,
                    touch_count: 1
                }
            ]
        );
    }

    #[test]
    fn resolves_relative_paths_against_record_cwd() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-relative-{}", std::process::id()));
        let project = root.join("repo");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        let transcript = root.join("session.jsonl");
        let mut file = File::create(&transcript).unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"cwd":"{}","type":"function_call","name":"edit","arguments":"{{\"file_path\":\"src/lib.rs\"}}"}}}}"#,
            project.display()
        )
        .unwrap();
        drop(file);

        let projects = parse_touched_projects(&transcript).unwrap();
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(
            projects,
            vec![ProjectTouchMetric {
                path: project,
                touch_count: 2
            }]
        );
    }
}
