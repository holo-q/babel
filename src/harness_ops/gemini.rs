use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    is_probably_text_state_file, text_file_contains_any, AdapterReadiness, HarnessMigrationReport,
    HarnessOpsContext, MigrationEdit, MAX_SCAN_BYTES, MAX_SCAN_FILES,
};

#[derive(Default)]
struct GeminiDiscovery {
    projects: BTreeMap<String, String>,
    matched_project_ids: BTreeSet<String>,
    matched_sessions: Vec<GeminiSession>,
    project_registry_refs: usize,
    settings_refs: usize,
    history_refs: usize,
    session_ref_files: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
}

struct GeminiSession {
    id: String,
    path: PathBuf,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let base = gemini_base(context);
    let tmp = gemini_tmp(context);
    let legacy_sessions = base.join("sessions");
    let projects_json = base.join("projects.json");
    let settings_json = base.join("settings.json");
    let history = base.join("history");

    let discovery = discover_gemini(
        &tmp,
        &legacy_sessions,
        &projects_json,
        &settings_json,
        &history,
        needles,
    )?;

    let mut state_roots = vec![
        base.clone(),
        tmp.clone(),
        legacy_sessions.clone(),
        projects_json.clone(),
        settings_json.clone(),
        history.clone(),
    ];
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    let source = old_path.display().to_string();
    let destination = new_path.display().to_string();
    let old_hash = sha256_hex(source.as_bytes());
    let new_hash = sha256_hex(destination.as_bytes());

    for root in [tmp.join(&old_hash), history.join(&old_hash)] {
        if root.exists() {
            let dest_root = root
                .parent()
                .map(|parent| parent.join(&new_hash))
                .unwrap_or_else(|| root.with_file_name(&new_hash));
            edits.push(
                MigrationEdit::rename_path(
                    AgentKind::Gemini,
                    "rename_project_hash_root",
                    root,
                    dest_root,
                    "preserve Gemini hash-keyed project state",
                )
                .with_apply_ready(true),
            );
        }
    }

    if !discovery.matched_sessions.is_empty() {
        let mut roots = discovery
            .matched_sessions
            .iter()
            .filter_map(|session| session.path.parent().and_then(Path::parent))
            .map(Path::to_path_buf)
            .collect::<BTreeSet<_>>();
        for project_id in &discovery.matched_project_ids {
            let project_root = tmp.join(project_id);
            if project_root.exists() {
                roots.insert(project_root);
            }
        }
        for root in roots {
            let (session_count, path_refs) = count_sessions_under_root(&root, needles)?;
            edits.push(MigrationEdit::preserve_session_keyed_files(
                AgentKind::Gemini,
                "preserve_project_session_root",
                root,
                session_count,
                path_refs,
            ));
        }
    }

    if discovery.project_registry_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Gemini,
                "rewrite_project_registry_refs",
                projects_json.display().to_string(),
                source.clone(),
                destination.clone(),
                discovery.project_registry_refs,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.settings_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Gemini,
                "rewrite_settings_refs",
                settings_json.display().to_string(),
                source.clone(),
                destination.clone(),
                discovery.settings_refs,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.history_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Gemini,
                "rewrite_history_refs",
                history.display().to_string(),
                source.clone(),
                destination.clone(),
                discovery.history_refs,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.session_ref_files > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Gemini,
                "rewrite_session_path_refs",
                tmp.display().to_string(),
                source,
                destination,
                discovery.session_ref_files,
            )
            .with_apply_ready(true),
        );
    }

    let mut notes = vec![
        "storage: ~/.gemini/tmp/<project-id>/chats plus legacy ~/.gemini/sessions".to_string(),
        "project identity: projects.json cwd mapping when present; SHA256(source path) is also probed".to_string(),
    ];

    for root in [&tmp, &legacy_sessions, &projects_json, &history] {
        if !root.exists() {
            notes.push(format!("state root missing: {}", root.display()));
        }
    }
    if discovery.truncated {
        notes.push(format!(
            "scan stopped after {} files; adapter needs a narrower pass before apply",
            discovery.files_scanned
        ));
    }
    if discovery.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large file(s) instead of full-reading them",
            discovery.large_files_sampled
        ));
    }
    if !discovery.matched_project_ids.is_empty() {
        notes.push(format!(
            "matched Gemini project id(s): {}",
            discovery
                .matched_project_ids
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !discovery.matched_sessions.is_empty() {
        notes.push(format!(
            "matched Gemini session id(s): {}",
            discovery
                .matched_sessions
                .iter()
                .take(4)
                .map(|session| format!("{} ({})", session.id, session.path.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let path_references_found = discovery.project_registry_refs
        + discovery.settings_refs
        + discovery.history_refs
        + discovery.session_ref_files;

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Gemini,
        AdapterReadiness::ApplyReady,
        state_roots,
        discovery.matched_sessions.len(),
        path_references_found,
        edits,
        notes,
    ))
}

fn gemini_base(context: &HarnessOpsContext) -> PathBuf {
    context.home.join(".gemini")
}

fn gemini_tmp(context: &HarnessOpsContext) -> PathBuf {
    context.gemini_tmp()
}

fn discover_gemini(
    tmp: &Path,
    legacy_sessions: &Path,
    projects_json: &Path,
    settings_json: &Path,
    history: &Path,
    needles: &[String],
) -> Result<GeminiDiscovery> {
    let mut discovery = GeminiDiscovery {
        projects: load_projects_json(projects_json)?,
        ..Default::default()
    };

    for (cwd, project_id) in &discovery.projects {
        if path_matches_needles(cwd, needles) {
            discovery.matched_project_ids.insert(project_id.clone());
        }
    }
    for needle in needles {
        if Path::new(needle).is_absolute() {
            discovery
                .matched_project_ids
                .insert(sha256_hex(needle.as_bytes()));
        }
    }

    discovery.project_registry_refs = text_file_ref_count(projects_json, needles)?;
    discovery.settings_refs = text_file_ref_count(settings_json, needles)?;
    discovery.history_refs = scan_tree_ref_count(history, needles, &mut discovery)?;

    discovery.session_ref_files = scan_tree_ref_count(tmp, needles, &mut discovery)?;
    collect_sessions_from_tmp(tmp, needles, &mut discovery)?;
    collect_legacy_sessions(legacy_sessions, needles, &mut discovery)?;

    Ok(discovery)
}

fn collect_sessions_from_tmp(
    tmp: &Path,
    needles: &[String],
    discovery: &mut GeminiDiscovery,
) -> Result<()> {
    if !tmp.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(tmp)? {
        let entry = entry?;
        let project_root = entry.path();
        if !project_root.is_dir() {
            continue;
        }
        let Some(project_id) = project_root
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let chats = project_root.join("chats");
        if !chats.exists() {
            continue;
        }
        collect_sessions_from_chats(&chats, &project_id, needles, discovery)?;
    }

    Ok(())
}

fn collect_sessions_from_chats(
    chats: &Path,
    project_id: &str,
    needles: &[String],
    discovery: &mut GeminiDiscovery,
) -> Result<()> {
    for entry in fs::read_dir(chats)? {
        if discovery.files_scanned >= MAX_SCAN_FILES {
            discovery.truncated = true;
            break;
        }

        let path = entry?.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || !is_gemini_session_file(&path) {
            continue;
        }

        discovery.files_scanned += 1;
        if let Some(session) = read_gemini_session_identity(&path, project_id, needles)? {
            discovery.matched_project_ids.insert(project_id.to_string());
            discovery.matched_sessions.push(session);
        }
    }
    Ok(())
}

fn collect_legacy_sessions(
    legacy_sessions: &Path,
    needles: &[String],
    discovery: &mut GeminiDiscovery,
) -> Result<()> {
    if !legacy_sessions.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(legacy_sessions)? {
        if discovery.files_scanned >= MAX_SCAN_FILES {
            discovery.truncated = true;
            break;
        }

        let path = entry?.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        discovery.files_scanned += 1;
        if text_file_contains_any(&path, metadata.len(), needles)? {
            discovery.session_ref_files += 1;
            if metadata.len() > MAX_SCAN_BYTES {
                discovery.large_files_sampled += 1;
            }
        }

        if let Some(session) = read_gemini_session_identity(&path, "legacy-sessions", needles)? {
            discovery.matched_sessions.push(session);
        }
    }
    Ok(())
}

fn read_gemini_session_identity(
    path: &Path,
    project_id: &str,
    needles: &[String],
) -> Result<Option<GeminiSession>> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
        return read_jsonl_session_identity(path, project_id, needles);
    }

    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(None);
    };

    session_from_value(path, project_id, &value, needles)
}

fn read_jsonl_session_identity(
    path: &Path,
    project_id: &str,
    needles: &[String],
) -> Result<Option<GeminiSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut state = serde_json::Map::new();
    let mut saw_message = false;

    for line in reader.lines().take(200) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(set) = value.get("$set").and_then(|value| value.as_object()) {
            for (key, value) in set {
                state.insert(key.clone(), value.clone());
            }
            continue;
        }
        if value.get("$rewindTo").is_some() {
            continue;
        }
        if value.get("type").is_some() || value.get("role").is_some() || value.get("id").is_some() {
            saw_message = true;
        }
        if let Some(object) = value.as_object() {
            for key in ["sessionId", "projectHash", "directories", "projectPath"] {
                if let Some(field) = object.get(key) {
                    state.insert(key.to_string(), field.clone());
                }
            }
        }
    }

    let mut value = serde_json::Value::Object(state);
    if saw_message {
        value["messages"] = serde_json::json!([{}]);
    }
    session_from_value(path, project_id, &value, needles)
}

fn session_from_value(
    path: &Path,
    project_id: &str,
    value: &serde_json::Value,
    needles: &[String],
) -> Result<Option<GeminiSession>> {
    let has_messages = value
        .get("messages")
        .and_then(|messages| messages.as_array())
        .is_some_and(|messages| !messages.is_empty());
    if !has_messages {
        return Ok(None);
    }

    let session_project = value
        .get("projectHash")
        .and_then(|value| value.as_str())
        .unwrap_or(project_id);
    let project_path = value
        .get("projectPath")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let directories_match = value
        .get("directories")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .any(|directory| path_matches_needles(directory, needles));

    let matched = path_matches_needles(project_path, needles)
        || directories_match
        || needles
            .iter()
            .any(|needle| sha256_hex(needle.as_bytes()) == session_project)
        || needles.iter().any(|needle| session_project == needle);
    if !matched {
        return Ok(None);
    }

    let id = value
        .get("sessionId")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| session_id_from_path(path));

    Ok(Some(GeminiSession {
        id,
        path: path.to_path_buf(),
    }))
}

fn load_projects_json(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    let content = fs::read_to_string(path)?;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(BTreeMap::new());
    };

    let Some(projects) = value.get("projects").and_then(|value| value.as_object()) else {
        return Ok(BTreeMap::new());
    };

    Ok(projects
        .iter()
        .filter_map(|(cwd, project_id)| {
            project_id
                .as_str()
                .map(|project_id| (cwd.clone(), project_id.to_string()))
        })
        .collect())
}

fn count_sessions_under_root(root: &Path, needles: &[String]) -> Result<(usize, usize)> {
    let mut session_count = 0;
    let mut path_refs = 0;
    if !root.exists() {
        return Ok((0, 0));
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                stack.push(entry?.path());
            }
            continue;
        }
        if !metadata.is_file() || !is_gemini_session_file(&path) {
            continue;
        }
        session_count += 1;
        if text_file_contains_any(&path, metadata.len(), needles)? {
            path_refs += 1;
        }
    }
    Ok((session_count, path_refs))
}

fn text_file_ref_count(path: &Path, needles: &[String]) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || !is_probably_text_state_file(path) {
        return Ok(0);
    }
    Ok(usize::from(text_file_contains_any(
        path,
        metadata.len(),
        needles,
    )?))
}

fn scan_tree_ref_count(
    root: &Path,
    needles: &[String],
    discovery: &mut GeminiDiscovery,
) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if discovery.files_scanned >= MAX_SCAN_FILES {
            discovery.truncated = true;
            break;
        }

        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                stack.push(entry?.path());
            }
            continue;
        }
        if !metadata.is_file() || !is_probably_text_state_file(&path) {
            continue;
        }

        discovery.files_scanned += 1;
        if text_file_contains_any(&path, metadata.len(), needles)? {
            count += 1;
            if metadata.len() > MAX_SCAN_BYTES {
                discovery.large_files_sampled += 1;
            }
        }
    }
    Ok(count)
}

fn path_matches_needles(path: &str, needles: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    needles.iter().any(|needle| {
        path == needle
            || path
                .strip_prefix(needle)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn is_gemini_session_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.starts_with("session-") {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json") | Some("jsonl")
    )
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix("session-"))
        .unwrap_or("unknown")
        .to_string()
}

fn sha256_hex(input: &[u8]) -> String {
    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut data = input.to_vec();
    let bit_len = (data.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    let mut h = H0;
    for chunk in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (idx, word) in w.iter_mut().take(16).enumerate() {
            let offset = idx * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for idx in 16..64 {
            let s0 =
                w[idx - 15].rotate_right(7) ^ w[idx - 15].rotate_right(18) ^ (w[idx - 15] >> 3);
            let s1 = w[idx - 2].rotate_right(17) ^ w[idx - 2].rotate_right(19) ^ (w[idx - 2] >> 10);
            w[idx] = w[idx - 16]
                .wrapping_add(s0)
                .wrapping_add(w[idx - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for idx in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[idx])
                .wrapping_add(w[idx]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"/data/projects/foo"),
            "383a7589d5d63244ba9ce9ad77c4c2ff5d51e4cf7b27c6706790e44fe5f9bba1"
        );
    }

    #[test]
    fn gemini_discovers_json_and_jsonl_sessions_for_source_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let source = home.join("work/project");
        let source_str = source.to_string_lossy().to_string();
        let project_hash = sha256_hex(source_str.as_bytes());
        let gemini = home.join(".gemini");
        let chats = gemini.join("tmp").join(&project_hash).join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::create_dir_all(gemini.join("history").join(&project_hash)).unwrap();
        fs::write(
            gemini.join("projects.json"),
            serde_json::json!({"projects": {source_str.clone(): project_hash.clone()}}).to_string(),
        )
        .unwrap();
        fs::write(
            chats.join("session-json.json"),
            serde_json::json!({
                "sessionId": "json-id",
                "projectHash": project_hash,
                "messages": [{"type": "user", "content": source_str}]
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            chats.join("session-jsonl.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"sessionId": "jsonl-id", "projectHash": project_hash}),
                serde_json::json!({"id": "m1", "type": "user", "content": source_str})
            ),
        )
        .unwrap();

        let context = HarnessOpsContext::from_home(home.to_path_buf());
        let dest = home.join("moved/project");
        let report = plan(&context, &source, &dest, &[source_str]).unwrap();

        assert_eq!(report.sessions_found, 2);
        assert!(report.path_references_found >= 2);
        assert!(report
            .edits
            .iter()
            .any(|edit| edit.action == "preserve_project_session_root"));
    }
}
