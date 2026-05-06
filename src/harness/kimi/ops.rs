use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::agent_kind::AgentKind;

use super::{AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit};

#[derive(Debug, Default)]
struct KimiDiscovery {
    share_dir: PathBuf,
    sessions_dir: PathBuf,
    config_path: PathBuf,
    path_keys: Vec<KimiPathKey>,
    matched_sessions: Vec<KimiSession>,
    config_path_refs: usize,
    session_path_refs: usize,
    path_references_found: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct KimiPathKey {
    old_workdir: PathBuf,
    new_workdir: PathBuf,
    old_key: String,
    new_key: String,
}

#[derive(Debug)]
struct KimiSession {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct KimiConfig {
    #[serde(default)]
    work_dirs: Vec<KimiWorkDir>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KimiWorkDir {
    Path(String),
    Entry { path: String, kaos: Option<String> },
}

impl KimiWorkDir {
    fn path(&self) -> Option<&str> {
        match self {
            KimiWorkDir::Path(path) if !path.is_empty() => Some(path),
            KimiWorkDir::Entry { path, .. } if !path.is_empty() => Some(path),
            _ => None,
        }
    }

    fn kaos(&self) -> Option<&str> {
        match self {
            KimiWorkDir::Entry { kaos, .. } => kaos.as_deref().filter(|value| !value.is_empty()),
            KimiWorkDir::Path(_) => None,
        }
    }
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, old_path, new_path, needles)?;
    let mut state_roots = vec![discovery.share_dir.clone(), discovery.sessions_dir.clone()];
    if discovery.config_path.exists() {
        state_roots.push(discovery.config_path.clone());
    }
    for path_key in &discovery.path_keys {
        let source_root = discovery.sessions_dir.join(&path_key.old_key);
        if source_root.exists() {
            state_roots.push(source_root);
        }
    }
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    for path_key in &discovery.path_keys {
        let source_root = discovery.sessions_dir.join(&path_key.old_key);
        let dest_root = discovery.sessions_dir.join(&path_key.new_key);
        if source_root.exists() {
            edits.push(
                MigrationEdit::rename_path(
                    AgentKind::Kimi,
                    "rename_workdir_session_root",
                    source_root,
                    dest_root,
                    format!(
                        "preserve Kimi session-id directories under workdir key {}",
                        path_key.old_key
                    ),
                )
                .with_apply_ready(true),
            );
        }
    }
    if discovery.config_path_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Kimi,
                "rewrite_workdir_registry",
                discovery.config_path.display().to_string(),
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.config_path_refs,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.session_path_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Kimi,
                "rewrite_session_path_refs",
                discovery.sessions_dir.display().to_string(),
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.session_path_refs,
            )
            .with_apply_ready(true),
        );
    }
    if !discovery.matched_sessions.is_empty() {
        edits.push(MigrationEdit::preserve_session_keyed_files(
            AgentKind::Kimi,
            "preserve_session_directories",
            discovery.sessions_dir.clone(),
            discovery.matched_sessions.len(),
            discovery.path_references_found,
        ));
    }

    let mut notes = vec![
        "storage: kimi.json work_dirs plus sessions/<workdir-hash>/<session-id>/".to_string(),
        "session id source: session directory name".to_string(),
    ];
    if !discovery.share_dir.exists() {
        notes.push(format!(
            "state root missing: {}",
            discovery.share_dir.display()
        ));
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
    if !discovery.path_keys.is_empty() {
        notes.push(format!(
            "planned Kimi workdir key move(s): {}",
            discovery
                .path_keys
                .iter()
                .map(|key| format!("{} -> {}", key.old_key, key.new_key))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !discovery.matched_sessions.is_empty() {
        let ids = discovery
            .matched_sessions
            .iter()
            .take(3)
            .map(|session| format!("{} ({})", session.id, session.path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if discovery.matched_sessions.len() > 3 {
            ", ..."
        } else {
            ""
        };
        notes.push(format!("matched Kimi session id(s): {ids}{suffix}"));
    }

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Kimi,
        AdapterReadiness::ApplyReady,
        state_roots,
        discovery.matched_sessions.len(),
        discovery.path_references_found,
        edits,
        notes,
    ))
}

fn discover(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<KimiDiscovery> {
    let share_dir = kimi_share_dir(context);
    let sessions_dir = share_dir.join("sessions");
    let config_path = share_dir.join("kimi.json");
    let path_keys = kimi_path_keys(&config_path, old_path, new_path)?;
    let config_scan = super::scan_text_refs(&config_path, needles)?;
    let session_scan = super::scan_text_refs(&sessions_dir, needles)?;
    let matched_sessions = collect_matched_sessions(&sessions_dir, &path_keys)?;
    let path_references_found =
        config_scan.path_references_found + session_scan.path_references_found;

    Ok(KimiDiscovery {
        share_dir,
        sessions_dir,
        config_path,
        path_keys,
        matched_sessions,
        config_path_refs: config_scan.path_references_found,
        session_path_refs: session_scan.path_references_found,
        path_references_found,
        files_scanned: config_scan.files_scanned + session_scan.files_scanned,
        truncated: config_scan.truncated || session_scan.truncated,
        large_files_sampled: config_scan.large_files_sampled + session_scan.large_files_sampled,
    })
}

fn kimi_share_dir(context: &HarnessOpsContext) -> PathBuf {
    std::env::var_os("KIMI_SHARE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| context.home.join(".kimi"))
}

fn kimi_path_keys(
    config_path: &Path,
    old_path: &Path,
    new_path: &Path,
) -> Result<Vec<KimiPathKey>> {
    let mut by_old_key = BTreeMap::new();
    insert_path_key(&mut by_old_key, old_path, new_path, None);

    for workdir in read_kimi_work_dirs(config_path)? {
        let Some(path) = workdir.path() else {
            continue;
        };
        let old_workdir = PathBuf::from(path);
        let Some(new_workdir) = rewrite_moved_path(&old_workdir, old_path, new_path) else {
            continue;
        };
        insert_path_key(&mut by_old_key, &old_workdir, &new_workdir, workdir.kaos());
    }

    Ok(by_old_key.into_values().collect())
}

fn read_kimi_work_dirs(config_path: &Path) -> Result<Vec<KimiWorkDir>> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(config_path)?;
    let Ok(config) = serde_json::from_str::<KimiConfig>(&content) else {
        return Ok(Vec::new());
    };
    Ok(config.work_dirs)
}

fn insert_path_key(
    by_old_key: &mut BTreeMap<String, KimiPathKey>,
    old_workdir: &Path,
    new_workdir: &Path,
    kaos: Option<&str>,
) {
    let old_md5 = md5_hex(old_workdir.to_string_lossy().as_bytes());
    let new_md5 = md5_hex(new_workdir.to_string_lossy().as_bytes());
    let old_key = kimi_workdir_key(&old_md5, kaos);
    let new_key = kimi_workdir_key(&new_md5, kaos);
    by_old_key.entry(old_key.clone()).or_insert(KimiPathKey {
        old_workdir: old_workdir.to_path_buf(),
        new_workdir: new_workdir.to_path_buf(),
        old_key,
        new_key,
    });
}

fn kimi_workdir_key(md5: &str, kaos: Option<&str>) -> String {
    match kaos {
        Some(kaos) if !kaos.eq_ignore_ascii_case("local") => format!("{kaos}_{md5}"),
        _ => md5.to_string(),
    }
}

fn rewrite_moved_path(path: &Path, old_path: &Path, new_path: &Path) -> Option<PathBuf> {
    if path == old_path {
        return Some(new_path.to_path_buf());
    }
    path.strip_prefix(old_path)
        .ok()
        .map(|relative| new_path.join(relative))
}

fn collect_matched_sessions(
    sessions_dir: &Path,
    path_keys: &[KimiPathKey],
) -> Result<Vec<KimiSession>> {
    let mut sessions = Vec::new();
    let mut seen = BTreeSet::new();
    for path_key in path_keys {
        let root = sessions_dir.join(&path_key.old_key);
        if !root.exists() {
            continue;
        }
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if seen.insert(path.clone()) {
                sessions.push(KimiSession {
                    id: id.to_string(),
                    path,
                });
            }
        }
    }
    Ok(sessions)
}

fn md5_hex(input: &[u8]) -> String {
    let digest = md5(input);
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut data = input.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0 = 0x67452301u32;
    let mut b0 = 0xefcdab89u32;
    let mut c0 = 0x98badcfeu32;
    let mut d0 = 0x10325476u32;

    for chunk in data.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            let start = i * 4;
            *word = u32::from_le_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }

        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | ((!b) & d), i)
            } else if i < 32 {
                ((d & b) | ((!d) & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | (!d)), (7 * i) % 16)
            };
            let next = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = d;
            d = c;
            c = b;
            b = next;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut digest = [0u8; 16];
    digest[0..4].copy_from_slice(&a0.to_le_bytes());
    digest[4..8].copy_from_slice(&b0.to_le_bytes());
    digest[8..12].copy_from_slice(&c0.to_le_bytes());
    digest[12..16].copy_from_slice(&d0.to_le_bytes());
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn kimi_doctor_reports_hash_root_and_text_rewrites() {
        let old_env = std::env::var_os("KIMI_SHARE_DIR");
        std::env::remove_var("KIMI_SHARE_DIR");
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let share = ctx.home.join(".kimi");
        let old_key = md5_hex(old.to_string_lossy().as_bytes());
        let session_dir = share.join("sessions").join(&old_key).join("session-kimi-1");
        write_file(
            &share.join("kimi.json"),
            &format!("{{\"work_dirs\":[{{\"path\":\"{}\"}}]}}\n", old.display()),
        );
        write_file(
            &session_dir.join("state.json"),
            &format!("{{\"cwd\":\"{}\"}}\n", old.display()),
        );
        write_file(
            &session_dir.join("wire.jsonl"),
            &format!(
                "{{\"message\":{{\"type\":\"ToolCall\",\"payload\":{{\"input\":{{\"file_path\":\"{}/src/lib.rs\"}}}}}}}}\n",
                old.display()
            ),
        );

        let report = plan(&ctx, &old, &new, &[old.display().to_string()]).unwrap();

        assert_eq!(report.harness, AgentKind::Kimi);
        assert!(matches!(report.readiness, AdapterReadiness::ApplyReady));
        assert_eq!(report.sessions_found, 1);
        assert!(report.path_references_found >= 2);
        assert!(report
            .edits
            .iter()
            .any(|edit| edit.action == "rename_workdir_session_root" && edit.apply_ready));
        assert!(report
            .edits
            .iter()
            .any(|edit| edit.action == "rewrite_workdir_registry" && edit.apply_ready));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("session directory name")));
        if let Some(value) = old_env {
            std::env::set_var("KIMI_SHARE_DIR", value);
        }
    }

    #[test]
    fn md5_matches_kimi_workdir_hash_reference() {
        assert_eq!(
            md5_hex(b"/tmp/project-share-dir"),
            "9d270ea5af83e326ca84eb9a1fd786ff"
        );
    }
}
