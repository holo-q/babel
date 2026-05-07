//! Cached transcript metrics for the resume pager.
//!
//! Incremental: on first load we parse the full transcript and cache the byte
//! offset. Subsequent loads seek to the cached offset and only parse new lines,
//! merging into the existing project-touch map. Full reparse only on file
//! truncation (session replaced or rotated).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::agent_kind::AgentKind;
use crate::babel_storage::BabelStorage;
use crate::session_row;

const PROJECT_CACHE_VERSION: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTouchMetric {
    pub path: PathBuf,
    pub touch_count: u32,
    pub ansi256: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkgroupStyle {
    pub root: PathBuf,
    pub ansi256: u8,
}

pub fn load_cached_session_projects(
    agent_kind: AgentKind,
    native_id: &str,
    session_key: &str,
) -> Result<Vec<ProjectTouchMetric>> {
    use crate::babel_storage::{ProjectCacheState, SessionProjectCacheEntry};

    let Some(path) = crate::harness::find_session_transcript(agent_kind, native_id)? else {
        anyhow::bail!("transcript not found");
    };

    let file_size = std::fs::metadata(&path)?.len();
    let source_path = format!("project-touch-v{PROJECT_CACHE_VERSION}:{}", path.display());
    let db = BabelStorage::open()?;

    let cached = db.get_project_cache_state(session_key, &source_path)?;

    // Determine if we can do an incremental parse or need full reparse
    let (mut metrics_map, start_offset) = match &cached {
        Some(state) if state.parsed_bytes == file_size => {
            // Fully up to date — return cached directly
            let mut projects: Vec<ProjectTouchMetric> = state
                .projects
                .iter()
                .map(|p| {
                    let path = PathBuf::from(&p.path);
                    ProjectTouchMetric {
                        ansi256: workgroup_ansi256_for_project(&path),
                        path,
                        touch_count: p.touch_count,
                    }
                })
                .collect();
            sort_touch_metrics_by_frequency(&mut projects);
            return Ok(projects);
        }
        Some(state) if state.parsed_bytes < file_size => {
            // File grew — incremental parse from offset
            let map: HashMap<PathBuf, ProjectAccumulator> = state
                .projects
                .iter()
                .enumerate()
                .map(|(idx, p)| {
                    (
                        PathBuf::from(&p.path),
                        ProjectAccumulator {
                            latest_line: idx,
                            touch_count: p.touch_count,
                        },
                    )
                })
                .collect();
            (map, state.parsed_bytes)
        }
        _ => {
            // No cache or file shrank (truncated/replaced) — full reparse
            (HashMap::new(), 0)
        }
    };

    // Parse from start_offset
    let file = File::open(&path)?;
    let mut reader = BufReader::new(file);
    if start_offset > 0 {
        reader.seek(SeekFrom::Start(start_offset))?;
    }

    let base_line = metrics_map
        .values()
        .map(|a| a.latest_line)
        .max()
        .unwrap_or(0);
    let mut line_buf = String::new();
    let mut line_index = base_line;

    loop {
        line_buf.clear();
        let bytes_read = reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        if line_buf.trim().is_empty() {
            continue;
        }

        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line_buf) else {
            continue;
        };

        let base_cwd = record_base_cwd(&record);
        let mut candidates = Vec::new();
        collect_path_candidates(&record, base_cwd.as_deref(), &mut candidates);

        line_index += 1;
        for candidate in candidates {
            let Some(project) = project_root_for_path(&candidate) else {
                continue;
            };
            let entry = metrics_map.entry(project).or_default();
            entry.latest_line = entry.latest_line.max(line_index);
            entry.touch_count = entry.touch_count.saturating_add(1);
        }
    }

    // Build sorted result
    let mut projects: Vec<(PathBuf, ProjectAccumulator)> = metrics_map.into_iter().collect();
    projects.sort_by(|(lp, la), (rp, ra)| {
        ra.touch_count
            .cmp(&la.touch_count)
            .then_with(|| ra.latest_line.cmp(&la.latest_line))
            .then_with(|| lp.cmp(rp))
    });

    let result: Vec<ProjectTouchMetric> = projects
        .iter()
        .map(|(path, metric)| ProjectTouchMetric {
            ansi256: workgroup_ansi256_for_project(path),
            path: path.clone(),
            touch_count: metric.touch_count,
        })
        .collect();

    // Persist incremental state
    let cache_entries: Vec<SessionProjectCacheEntry> = result
        .iter()
        .map(|p| SessionProjectCacheEntry {
            path: p.path.display().to_string(),
            touch_count: p.touch_count,
            ansi256: p.ansi256,
        })
        .collect();

    let _ = db.set_project_cache_state(
        session_key,
        &source_path,
        &ProjectCacheState {
            projects: cache_entries,
            parsed_bytes: file_size,
        },
    );

    Ok(result)
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
            let Some(project) = project_root_for_path(&candidate) else {
                continue;
            };
            let entry = metrics_by_project.entry(project).or_default();
            entry.latest_line = entry.latest_line.max(line_index);
            entry.touch_count = entry.touch_count.saturating_add(1);
        }
    }

    let mut projects: Vec<(PathBuf, ProjectAccumulator)> = metrics_by_project.into_iter().collect();
    projects.sort_by(|(left_path, left), (right_path, right)| {
        right
            .touch_count
            .cmp(&left.touch_count)
            .then_with(|| right.latest_line.cmp(&left.latest_line))
            .then_with(|| left_path.cmp(right_path))
    });

    Ok(projects
        .into_iter()
        .map(|(path, metric)| ProjectTouchMetric {
            ansi256: workgroup_ansi256_for_project(&path),
            path,
            touch_count: metric.touch_count,
        })
        .collect())
}

fn sort_touch_metrics_by_frequency(projects: &mut [ProjectTouchMetric]) {
    projects.sort_by(|left, right| {
        right
            .touch_count
            .cmp(&left.touch_count)
            .then_with(|| left.path.cmp(&right.path))
    });
}

#[derive(Debug, Clone, Copy, Default)]
struct ProjectAccumulator {
    latest_line: usize,
    touch_count: u32,
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
                if matches!(key.as_str(), "input" | "arguments" | "params") {
                    collect_tool_payload_paths(child, base, out);
                    continue;
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

fn collect_tool_payload_paths(value: &Value, base: Option<&Path>, out: &mut Vec<PathBuf>) {
    if let Some(parsed) = value
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    {
        collect_tool_payload_paths(&parsed, base, out);
        return;
    }

    match value {
        Value::Object(map) => {
            let payload_base = first_string_for_keys(value, &["cwd", "workdir"])
                .and_then(|cwd| candidate_path(cwd, base))
                .or_else(|| base.map(Path::to_path_buf));
            for (key, child) in map {
                if is_path_key(key) {
                    collect_path_value(child, payload_base.as_deref(), out);
                } else if matches!(key.as_str(), "input" | "arguments" | "params") {
                    collect_tool_payload_paths(child, payload_base.as_deref(), out);
                } else {
                    collect_tool_payload_paths(child, payload_base.as_deref(), out);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_tool_payload_paths(value, base, out);
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

fn project_root_for_path(path: &Path) -> Option<PathBuf> {
    let normalized = normalize_lexical_path(path);
    let mut candidate =
        if normalized.is_file() || (!normalized.exists() && normalized.extension().is_some()) {
            normalized.parent().unwrap_or(&normalized).to_path_buf()
        } else {
            normalized
        };

    candidate = normalize_lexical_path(&candidate);
    // Touched-project identity is meant to answer "which project was this
    // session working in?", not "which package manifest happened to own this
    // file?". Prefer stable repository/workspace roots before package markers
    // so Cargo member crates like `mogitor/crates/mogitor-watch` fold back to
    // the `mogitor` workspace row by default.
    nearest_ancestor_matching(&candidate, is_git_root)
        .or_else(|| nearest_ancestor_matching(&candidate, is_workspace_root))
        .or_else(|| nearest_ancestor_matching(&candidate, is_package_project_root))
}

fn nearest_ancestor_matching(path: &Path, predicate: fn(&Path) -> bool) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| predicate(ancestor))
        .map(Path::to_path_buf)
}

fn is_git_root(path: &Path) -> bool {
    path.join(".git").exists()
}

fn is_workspace_root(path: &Path) -> bool {
    has_cargo_workspace(path)
        || path.join("pnpm-workspace.yaml").exists()
        || package_json_declares_workspaces(path)
        || path.join("settings.gradle").exists()
        || path.join("settings.gradle.kts").exists()
        || has_solution_file(path)
}

fn is_package_project_root(path: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "pyproject.toml",
        "Cargo.toml",
        "package.json",
        "yarn.lock",
        "deno.json",
        "go.mod",
        "build.gradle",
        "build.gradle.kts",
        "pom.xml",
        "mix.exs",
        "gleam.toml",
        "Package.swift",
        "workgroup.toml",
    ];

    MARKERS.iter().any(|marker| path.join(marker).exists())
}

fn has_cargo_workspace(path: &Path) -> bool {
    let manifest = path.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return false;
    };
    text.parse::<toml::Value>()
        .ok()
        .and_then(|value| value.get("workspace").cloned())
        .is_some()
}

fn package_json_declares_workspaces(path: &Path) -> bool {
    let manifest = path.join("package.json");
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("workspaces").cloned())
        .is_some()
}

fn has_solution_file(path: &Path) -> bool {
    std::fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "sln"))
}

pub fn workgroup_style_for_path(path: &Path) -> Option<WorkgroupStyle> {
    let scope = nearest_workgroup_scope(path)?;
    Some(WorkgroupStyle {
        root: scope.root,
        ansi256: scope
            .ansi256
            .unwrap_or_else(|| auto_workgroup_ansi256(&scope.name)),
    })
}

fn workgroup_ansi256_for_project(path: &Path) -> Option<u8> {
    workgroup_style_for_path(path).map(|style| style.ansi256)
}

#[derive(Debug)]
struct WorkgroupScope {
    root: PathBuf,
    name: String,
    ansi256: Option<u8>,
}

fn nearest_workgroup_scope(path: &Path) -> Option<WorkgroupScope> {
    for ancestor in path.ancestors() {
        let workgroup_path = ancestor.join("workgroup.toml");
        if !workgroup_path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&workgroup_path).ok()?;
        let value = text.parse::<toml::Value>().ok()?;
        let table = value.get("workgroup").unwrap_or(&value);
        let name = table
            .get("name")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                ancestor
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })?;
        let ansi256 = first_workgroup_ansi(table);
        return Some(WorkgroupScope {
            root: ancestor.to_path_buf(),
            name,
            ansi256,
        });
    }
    None
}

fn first_workgroup_ansi(value: &toml::Value) -> Option<u8> {
    for key in ["ansi256", "ansi", "ansi_color"] {
        if let Some(ansi) = value.get(key).and_then(toml_value_to_ansi256) {
            return Some(ansi);
        }
    }
    for key in ["color", "fg", "foreground"] {
        if let Some(ansi) = value
            .get(key)
            .and_then(toml::Value::as_str)
            .and_then(color_text_to_ansi256)
        {
            return Some(ansi);
        }
    }
    None
}

fn toml_value_to_ansi256(value: &toml::Value) -> Option<u8> {
    match value {
        toml::Value::Integer(i) => u8::try_from(*i).ok(),
        toml::Value::String(text) => color_text_to_ansi256(text),
        _ => None,
    }
}

fn color_text_to_ansi256(text: &str) -> Option<u8> {
    let text = text.trim();
    if let Ok(ansi) = text.parse::<u8>() {
        return Some(ansi);
    }
    if text.starts_with('#') {
        return Some(session_row::theme_balanced_ansi256_from_hex(text));
    }
    match text.to_ascii_lowercase().as_str() {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "bright_black" | "gray" | "grey" => Some(8),
        "bright_red" => Some(9),
        "bright_green" => Some(10),
        "bright_yellow" => Some(11),
        "bright_blue" => Some(12),
        "bright_magenta" => Some(13),
        "bright_cyan" => Some(14),
        "bright_white" => Some(15),
        _ => None,
    }
}

fn auto_workgroup_ansi256(name: &str) -> u8 {
    const PALETTE: &[u8] = &[
        33, 39, 45, 69, 75, 81, 111, 117, 141, 147, 177, 183, 209, 215,
    ];
    let hash = name.bytes().fold(0usize, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(byte as usize)
    });
    PALETTE[hash % PALETTE.len()]
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

    fn write_test_workgroup(root: &Path) {
        std::fs::write(
            root.join("workgroup.toml"),
            "[workgroup]\nname = \"project-metrics-test\"\nansi256 = 39\n",
        )
        .unwrap();
    }

    #[test]
    fn extracts_latest_touched_projects_from_tool_inputs() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        write_test_workgroup(&root);
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
                    touch_count: 2,
                    ansi256: Some(39)
                },
                ProjectTouchMetric {
                    path: beta,
                    touch_count: 1,
                    ansi256: Some(39)
                }
            ]
        );
    }

    #[test]
    fn touched_projects_are_sorted_by_frequency_before_recency() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-frequency-{}", std::process::id()));
        let alpha = root.join("workspace");
        let beta = root.join("wnck-sys");
        std::fs::create_dir_all(alpha.join(".git")).unwrap();
        std::fs::create_dir_all(beta.join(".git")).unwrap();
        let transcript = root.join("session.jsonl");
        let mut file = File::create(&transcript).unwrap();

        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","arguments":"{{\"file_path\":\"{}/src/lib.rs\"}}"}}}}"#,
            beta.display()
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","arguments":"{{\"file_path\":\"{}/src/ffi.rs\"}}"}}}}"#,
            beta.display()
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","arguments":"{{\"file_path\":\"{}/README.md\"}}"}}}}"#,
            alpha.display()
        )
        .unwrap();
        drop(file);

        let projects = parse_touched_projects(&transcript).unwrap();
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(projects[0].path, beta);
        assert_eq!(projects[0].touch_count, 2);
        assert_eq!(projects[1].path, alpha);
        assert_eq!(projects[1].touch_count, 1);
    }

    #[test]
    fn git_root_beats_nested_package_markers() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-git-root-{}", std::process::id()));
        let repo = root.join("mogitor");
        let crate_dir = repo.join("crates").join("mogitor-watch");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"mogitor-watch\"\n",
        )
        .unwrap();

        let root_for_member = project_root_for_path(&crate_dir.join("src/lib.rs"));
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(root_for_member, Some(repo));
    }

    #[test]
    fn cargo_workspace_root_beats_member_manifest_without_git() {
        let root = std::env::current_dir().unwrap().join("tmp").join(format!(
            "project-metrics-cargo-workspace-{}",
            std::process::id()
        ));
        let workspace = root.join("mogitor");
        let crate_dir = workspace.join("crates").join("mogitor-domain");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"mogitor-domain\"\n",
        )
        .unwrap();

        let root_for_member = project_root_for_path(&crate_dir.join("src/lib.rs"));
        std::fs::remove_dir_all(root).unwrap();

        assert_eq!(root_for_member, Some(workspace));
    }

    #[test]
    fn resolves_relative_paths_against_record_cwd() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-relative-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        write_test_workgroup(&root);
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
                touch_count: 1,
                ansi256: Some(39)
            }]
        );
    }

    #[test]
    fn ignores_path_like_tool_output_without_project_markers() {
        let root = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("project-metrics-output-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        write_test_workgroup(&root);
        let project = root.join("repo");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        let transcript = root.join("session.jsonl");
        let mut file = File::create(&transcript).unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","arguments":"{{\"file_path\":\"src/lib.rs\",\"cwd\":\"{}\"}}","output":{{"path":"/usr/share/xdg-desktop-portal"}}}}}}"#,
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
                touch_count: 2,
                ansi256: Some(39)
            }]
        );
    }
}
