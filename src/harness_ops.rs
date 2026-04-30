//! Harness-aware operations planning.
//!
//! This is the read side of the "consume the boards" plan: Babel can inspect
//! provider-native state and produce an operation graph without making a global
//! search index the source of truth. Native harness storage remains authoritative;
//! any future cache must be rebuildable from these adapters.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agent_kind::{AgentKind, HarnessSupport};
use crate::core::ConflictingPane;

mod codex;

const MAX_SCAN_FILES: usize = 5_000;
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;
const LARGE_FILE_SAMPLE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct HarnessOpsContext {
    pub home: PathBuf,
}

impl HarnessOpsContext {
    pub fn from_home(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn system() -> Result<Self> {
        Ok(Self {
            home: dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?,
        })
    }

    fn claude_base(&self) -> PathBuf {
        self.home.join(".claude")
    }

    fn codex_base(&self) -> PathBuf {
        self.home.join(".codex")
    }

    fn codex_sessions(&self) -> PathBuf {
        self.home.join(".codex/sessions")
    }

    fn codex_archived_sessions(&self) -> PathBuf {
        self.home.join(".codex/archived_sessions")
    }

    fn codex_shell_snapshots(&self) -> PathBuf {
        self.home.join(".codex/shell_snapshots")
    }

    fn qwen_base(&self) -> PathBuf {
        self.home.join(".qwen")
    }

    fn gemini_tmp(&self) -> PathBuf {
        self.home.join(".gemini/tmp")
    }

    fn cursor_roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".cursor/projects"),
            self.home.join(".cursor/chats"),
            self.home
                .join(".config/Cursor/User/globalStorage/state.vscdb"),
            self.home.join(".config/Cursor/User/workspaceStorage"),
        ]
    }

    fn vscode_roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".config/Code/User/globalStorage"),
            self.home.join(".config/Code/User/workspaceStorage"),
        ]
    }

    fn opencode_roots(&self) -> Vec<PathBuf> {
        vec![self.home.join(".local/share/opencode/opencode.db")]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterReadiness {
    /// A future apply engine has a complete transaction contract for this adapter.
    ApplyReady,
    /// The adapter can produce a useful operation graph, but does not mutate.
    DoctorOnly,
    /// Babel knows where to look, but path rewrite semantics are not specified.
    ReconOnly,
    /// No credible storage migration surface is known.
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskSeverity {
    Info,
    Warning,
    Blocker,
}

impl RiskSeverity {
    pub fn label(&self) -> &'static str {
        match self {
            RiskSeverity::Info => "info",
            RiskSeverity::Warning => "warn",
            RiskSeverity::Blocker => "blocker",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRisk {
    pub severity: RiskSeverity,
    pub harness: Option<AgentKind>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedOperation {
    pub harness: AgentKind,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub apply_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessMigrationReport {
    pub harness: AgentKind,
    pub readiness: AdapterReadiness,
    pub state_roots: Vec<PathBuf>,
    pub sessions_found: usize,
    pub path_references_found: usize,
    pub operations: Vec<PlannedOperation>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivePaneImpact {
    pub pane_id: u64,
    pub socket: String,
    pub harness: AgentKind,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub relative_path: PathBuf,
    pub state: String,
    pub migratable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationDoctorReport {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub indexing_policy: String,
    pub live_panes: Vec<LivePaneImpact>,
    pub harnesses: Vec<HarnessMigrationReport>,
    pub risks: Vec<MigrationRisk>,
}

impl MigrationDoctorReport {
    pub fn operations(&self) -> Vec<&PlannedOperation> {
        self.harnesses
            .iter()
            .flat_map(|harness| harness.operations.iter())
            .collect()
    }

    pub fn has_blockers(&self) -> bool {
        self.risks
            .iter()
            .any(|risk| matches!(risk.severity, RiskSeverity::Blocker))
    }

    pub fn warning_count(&self) -> usize {
        self.risks
            .iter()
            .filter(|risk| matches!(risk.severity, RiskSeverity::Warning))
            .count()
    }
}

pub fn live_panes_from_conflicts(conflicts: &[ConflictingPane]) -> Vec<LivePaneImpact> {
    conflicts
        .iter()
        .map(|conflict| {
            let state = format!("{:?}", conflict.state);
            let migratable = matches!(
                conflict.state,
                crate::ActivityState::Idle
                    | crate::ActivityState::AwaitingInput
                    | crate::ActivityState::PlanApproval
                    | crate::ActivityState::Unknown
            );
            LivePaneImpact {
                pane_id: conflict.pane.id(),
                socket: conflict.pane.socket().to_string(),
                harness: conflict.pane.agent_kind,
                session_id: conflict.pane.session_id.clone(),
                cwd: conflict.pane.cwd.clone(),
                relative_path: conflict.relative_path.clone(),
                state,
                migratable,
            }
        })
        .collect()
}

pub fn plan_migration(
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    let context = HarnessOpsContext::system()?;
    plan_migration_with_context(&context, old_path, new_path, live_panes)
}

pub fn plan_migration_with_context(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    let old_abs = absolute_path(old_path);
    let new_abs = absolute_path(new_path);
    let path_needles = path_needles(old_path, &old_abs);

    let mut risks = Vec::new();
    if old_abs == new_abs {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: None,
            message: "source and destination resolve to the same path".to_string(),
        });
    }
    if new_abs.starts_with(&old_abs) {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: None,
            message: "destination is nested inside source".to_string(),
        });
    }

    for pane in &live_panes {
        if !pane.migratable {
            risks.push(MigrationRisk {
                severity: RiskSeverity::Blocker,
                harness: Some(pane.harness),
                message: format!(
                    "pane {} is active ({}) and would break during migration",
                    pane.pane_id, pane.state
                ),
            });
        }
    }

    let mut harnesses = Vec::new();
    harnesses.push(plan_claude(
        context,
        &old_abs,
        &new_abs,
        &path_needles,
        &mut risks,
    )?);
    harnesses.push(codex::plan(context, &old_abs, &new_abs, &path_needles)?);
    harnesses.push(plan_text_storage_harness(
        context.gemini_tmp(),
        AgentKind::Gemini,
        &path_needles,
        "Gemini project identity is hash/path based; doctor reports references before apply exists.",
    )?);
    harnesses.push(plan_text_storage_harness(
        context.qwen_base(),
        AgentKind::QwenCode,
        &path_needles,
        "Qwen has compatible hook identity, but path-move storage rewrite still needs native fixtures.",
    )?);
    harnesses.push(plan_cursor(context));
    harnesses.push(plan_shared_vscode_harness(
        context,
        AgentKind::Cline,
        "Cline task history lives in VS Code extension storage; close the IDE before any future migration.",
    ));
    harnesses.push(plan_shared_vscode_harness(
        context,
        AgentKind::RooCode,
        "Roo has no lifecycle hooks today, but its VS Code storage may still need preservation on project moves.",
    ));
    harnesses.push(plan_shared_vscode_harness(
        context,
        AgentKind::KiloCode,
        "Kilo has no lifecycle hooks today, but its VS Code storage may still need preservation on project moves.",
    ));
    harnesses.push(plan_roots_only_harness(
        AgentKind::OpenCode,
        context.opencode_roots(),
        "OpenCode uses an in-process plugin model and local database storage; no mutation adapter exists.",
    ));
    harnesses.push(plan_project_local_harness(AgentKind::Aider));

    for kind in AgentKind::ALL {
        if harnesses.iter().any(|report| report.harness == *kind) {
            continue;
        }
        harnesses.push(HarnessMigrationReport {
            harness: *kind,
            readiness: AdapterReadiness::Unsupported,
            state_roots: Vec::new(),
            sessions_found: 0,
            path_references_found: 0,
            operations: Vec::new(),
            notes: vec![
                "no path-move adapter has been extracted from references yet; doctor keeps this explicit instead of guessing"
                    .to_string(),
            ],
        });
    }

    risks.push(MigrationRisk {
        severity: RiskSeverity::Info,
        harness: None,
        message:
            "no cross-harness full-text index is used; all counts come from native storage scans"
                .to_string(),
    });

    Ok(MigrationDoctorReport {
        old_path: old_abs,
        new_path: new_abs,
        indexing_policy:
            "native storage is source of truth; indexing is deferred and must remain rebuildable"
                .to_string(),
        live_panes,
        harnesses,
        risks,
    })
}

fn plan_claude(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
    risks: &mut Vec<MigrationRisk>,
) -> Result<HarnessMigrationReport> {
    let projects_dir = context.claude_base().join("projects");
    let source_candidates = claude_project_candidates(&projects_dir, old_path);
    let dest_candidates = claude_project_candidates(&projects_dir, new_path);
    let history_path = context.claude_base().join("history.jsonl");
    let session_keyed_roots = claude_session_keyed_roots(context);
    let user_wide_files = claude_user_wide_files(context);

    let existing_sources: Vec<_> = source_candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
        .collect();
    let sessions_found = existing_sources.iter().try_fold(0, |count, candidate| {
        Ok::<_, anyhow::Error>(count + count_jsonl_files(&candidate.path)?)
    })?;
    let history_refs = count_history_refs(&history_path, old_path)?;
    let session_refs = session_keyed_roots.iter().try_fold(0, |count, root| {
        Ok::<_, anyhow::Error>(count + scan_text_refs(root, needles)?.path_references_found)
    })?;
    let user_wide_refs = user_wide_files.iter().try_fold(0, |count, file| {
        Ok::<_, anyhow::Error>(count + scan_text_refs(file, needles)?.path_references_found)
    })?;
    let mut operations = Vec::new();
    let mut notes = Vec::new();

    for source in &existing_sources {
        let Some(dest) = dest_candidates
            .iter()
            .find(|candidate| candidate.scheme == source.scheme)
            .or_else(|| dest_candidates.first())
        else {
            continue;
        };
        operations.push(PlannedOperation {
            harness: AgentKind::Claude,
            action: "rename_project_dir".to_string(),
            target: format!("{} -> {}", source.path.display(), dest.path.display()),
            detail: format!(
                "{} key; preserve {} Claude transcript file(s)",
                source.scheme, sessions_found
            ),
            apply_ready: false,
        });
    }

    if existing_sources.is_empty() {
        notes.push(format!(
            "Claude project directory not found; probed keys: {}",
            source_candidates
                .iter()
                .map(|candidate| candidate.encoded.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if history_refs > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Claude,
            action: "rewrite_history_paths".to_string(),
            target: history_path.display().to_string(),
            detail: format!("rewrite {} history entrie(s)", history_refs),
            apply_ready: false,
        });
    }

    if session_refs > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Claude,
            action: "rewrite_session_keyed_refs".to_string(),
            target: "~/.claude/{todos,usage-data,plugins/data,tasks}".to_string(),
            detail: format!("rewrite {} session-keyed file(s)", session_refs),
            apply_ready: false,
        });
    }

    if user_wide_refs > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Claude,
            action: "rewrite_user_wide_refs".to_string(),
            target: "~/.claude.json and Claude plugin/settings files".to_string(),
            detail: format!("rewrite {} user-wide file(s)", user_wide_refs),
            apply_ready: false,
        });
    }

    for dest in dest_candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
    {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: Some(AgentKind::Claude),
            message: format!(
                "Claude destination project folder already exists for {} key: {}",
                dest.scheme,
                dest.path.display()
            ),
        });
    }

    if source_candidates
        .iter()
        .zip(dest_candidates.iter())
        .any(|(old, new)| old.encoded == new.encoded)
    {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: Some(AgentKind::Claude),
            message: "Claude source and destination can encode to the same project key".to_string(),
        });
    }

    notes.push(
        "Claude apply is deliberately not wired to legacy babel mv; references require copy/verify/rewrite/rollback before mutation is trusted."
            .to_string(),
    );
    notes.push(
        "cc-port covers history, transcripts, settings, todos, usage-data, plugins/data, tasks, and opaque file-history preservation."
            .to_string(),
    );

    let mut state_roots = vec![context.claude_base(), context.home.join(".claude.json")];
    state_roots.extend(session_keyed_roots);
    state_roots.extend(user_wide_files);
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    Ok(HarnessMigrationReport {
        harness: AgentKind::Claude,
        readiness: AdapterReadiness::DoctorOnly,
        state_roots,
        sessions_found,
        path_references_found: history_refs + session_refs + user_wide_refs,
        operations,
        notes,
    })
}

fn plan_text_storage_harness(
    root: PathBuf,
    harness: AgentKind,
    needles: &[String],
    note: &str,
) -> Result<HarnessMigrationReport> {
    let scan = scan_text_refs(&root, needles)?;
    let mut operations = Vec::new();
    let mut notes = vec![note.to_string()];

    if !root.exists() {
        notes.push(format!("state root missing: {}", root.display()));
    } else if scan.truncated {
        notes.push(format!(
            "scan stopped after {} files; use a narrower adapter before applying",
            scan.files_scanned
        ));
    }
    if scan.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large file(s) instead of full-reading them",
            scan.large_files_sampled
        ));
    }

    if scan.path_references_found > 0 {
        operations.push(PlannedOperation {
            harness,
            action: "rewrite_native_path_refs".to_string(),
            target: root.display().to_string(),
            detail: format!(
                "{} file(s) contain source path references",
                scan.path_references_found
            ),
            apply_ready: false,
        });
    }

    Ok(HarnessMigrationReport {
        harness,
        readiness: AdapterReadiness::DoctorOnly,
        state_roots: vec![root],
        sessions_found: 0,
        path_references_found: scan.path_references_found,
        operations,
        notes,
    })
}

fn plan_cursor(context: &HarnessOpsContext) -> HarnessMigrationReport {
    let roots: Vec<PathBuf> = context
        .cursor_roots()
        .into_iter()
        .filter(|root| root.exists())
        .collect();

    let mut notes = vec![
        "Cursor is reconnaissance-only until workspaceStorage/globalStorage fixtures exist."
            .to_string(),
        "cursor-chat-recovery-kit closes Cursor, backs up current state.vscdb, copies old state.vscdb/images, then restarts Cursor."
            .to_string(),
    ];
    if roots.is_empty() {
        notes.push("no Cursor state roots detected".to_string());
    }

    HarnessMigrationReport {
        harness: AgentKind::Cursor,
        readiness: AdapterReadiness::ReconOnly,
        state_roots: roots,
        sessions_found: 0,
        path_references_found: 0,
        operations: Vec::new(),
        notes,
    }
}

fn plan_shared_vscode_harness(
    context: &HarnessOpsContext,
    harness: AgentKind,
    note: &str,
) -> HarnessMigrationReport {
    plan_roots_only_harness(harness, context.vscode_roots(), note)
}

fn plan_roots_only_harness(
    harness: AgentKind,
    roots: Vec<PathBuf>,
    note: &str,
) -> HarnessMigrationReport {
    let existing_roots: Vec<PathBuf> = roots.into_iter().filter(|root| root.exists()).collect();
    let mut notes = vec![note.to_string()];
    if existing_roots.is_empty() {
        notes.push("no known state roots detected".to_string());
    }

    HarnessMigrationReport {
        harness,
        readiness: AdapterReadiness::ReconOnly,
        state_roots: existing_roots,
        sessions_found: 0,
        path_references_found: 0,
        operations: Vec::new(),
        notes,
    }
}

fn plan_project_local_harness(harness: AgentKind) -> HarnessMigrationReport {
    HarnessMigrationReport {
        harness,
        readiness: AdapterReadiness::DoctorOnly,
        state_roots: Vec::new(),
        sessions_found: 0,
        path_references_found: 0,
        operations: vec![PlannedOperation {
            harness,
            action: "preserve_project_local_history".to_string(),
            target: "source directory contents".to_string(),
            detail: "project-local chat/history files should move with the project itself".to_string(),
            apply_ready: false,
        }],
        notes: vec![
            "Aider is mostly a filesystem move problem; no global session rewrite adapter is expected for v1."
                .to_string(),
        ],
    }
}

#[derive(Debug)]
struct ClaudeProjectCandidate {
    scheme: &'static str,
    encoded: String,
    path: PathBuf,
}

fn claude_project_candidates(
    projects_dir: &Path,
    project_path: &Path,
) -> Vec<ClaudeProjectCandidate> {
    let mut candidates = Vec::new();
    let cc_port = claude_encode_cc_port(project_path);
    candidates.push(ClaudeProjectCandidate {
        scheme: "cc-port",
        path: projects_dir.join(&cc_port),
        encoded: cc_port,
    });

    let ccmv = claude_encode_ccmv(project_path);
    if candidates.iter().all(|candidate| candidate.encoded != ccmv) {
        candidates.push(ClaudeProjectCandidate {
            scheme: "ccmv",
            path: projects_dir.join(&ccmv),
            encoded: ccmv,
        });
    }

    candidates
}

fn claude_encode_cc_port(path: &Path) -> String {
    normalized_path_for_key(path)
        .replace('/', "-")
        .replace('.', "-")
        .replace(' ', "-")
}

fn claude_encode_ccmv(path: &Path) -> String {
    let normalized = normalized_path_for_key(path);
    normalized
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn normalized_path_for_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.len() > 1 {
        raw.trim_end_matches('/').to_string()
    } else {
        raw.to_string()
    }
}

fn claude_session_keyed_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.claude_base().join("todos"),
        context.claude_base().join("usage-data/session-meta"),
        context.claude_base().join("usage-data/facets"),
        context.claude_base().join("plugins/data"),
        context.claude_base().join("tasks"),
    ]
}

fn claude_user_wide_files(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.home.join(".claude.json"),
        context.claude_base().join("settings.json"),
        context.claude_base().join("plugins/installed_plugins.json"),
        context
            .claude_base()
            .join("plugins/known_marketplaces.json"),
    ]
}

fn count_jsonl_files(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
            count += 1;
        }
    }
    Ok(count)
}

fn count_history_refs(history_path: &Path, old_path: &Path) -> Result<usize> {
    if !history_path.exists() {
        return Ok(0);
    }

    let content = fs::read_to_string(history_path)?;
    let old_path = old_path.to_string_lossy();
    let child_prefix = format!("{}/", old_path);
    let mut count = 0;

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(project) = value.get("project").and_then(|v| v.as_str()) else {
            continue;
        };
        if project == old_path || project.starts_with(&child_prefix) {
            count += 1;
        }
    }
    Ok(count)
}

#[derive(Default)]
struct TextScan {
    files_scanned: usize,
    path_references_found: usize,
    truncated: bool,
    large_files_sampled: usize,
}

fn scan_text_refs(root: &Path, needles: &[String]) -> Result<TextScan> {
    let mut scan = TextScan::default();
    if !root.exists() {
        return Ok(scan);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if scan.files_scanned >= MAX_SCAN_FILES {
            scan.truncated = true;
            break;
        }

        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                let entry = entry?;
                stack.push(entry.path());
            }
            continue;
        }
        if !metadata.is_file() || !is_probably_text_state_file(&path) {
            continue;
        }

        scan.files_scanned += 1;
        let Ok(found) = text_file_contains_any(&path, metadata.len(), needles) else {
            continue;
        };
        if metadata.len() > MAX_SCAN_BYTES {
            scan.large_files_sampled += 1;
        }
        if found {
            scan.path_references_found += 1;
        }
    }

    Ok(scan)
}

fn text_file_contains_any(path: &Path, len: u64, needles: &[String]) -> Result<bool> {
    if len <= MAX_SCAN_BYTES {
        let content = fs::read_to_string(path)?;
        return Ok(needles.iter().any(|needle| content.contains(needle)));
    }

    let mut file = fs::File::open(path)?;
    let mut head = vec![0; LARGE_FILE_SAMPLE_BYTES.min(len as usize)];
    let head_len = file.read(&mut head)?;
    head.truncate(head_len);
    if contains_any_bytes(&head, needles) {
        return Ok(true);
    }

    if len > LARGE_FILE_SAMPLE_BYTES as u64 {
        let tail_start = len.saturating_sub(LARGE_FILE_SAMPLE_BYTES as u64);
        file.seek(SeekFrom::Start(tail_start))?;
        let mut tail = Vec::with_capacity(LARGE_FILE_SAMPLE_BYTES);
        file.take(LARGE_FILE_SAMPLE_BYTES as u64)
            .read_to_end(&mut tail)?;
        if contains_any_bytes(&tail, needles) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn contains_any_bytes(bytes: &[u8], needles: &[String]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    needles.iter().any(|needle| text.contains(needle))
}

fn is_probably_text_state_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json") | Some("jsonl") | Some("toml") | Some("txt") | Some("md")
    )
}

fn absolute_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn path_needles(original: &Path, absolute: &Path) -> Vec<String> {
    let mut needles = BTreeSet::new();
    needles.insert(absolute.to_string_lossy().to_string());
    if original.is_absolute() {
        needles.insert(original.to_string_lossy().to_string());
    }
    needles.into_iter().collect()
}

pub fn supported_operation_harnesses() -> Vec<AgentKind> {
    AgentKind::ALL
        .iter()
        .copied()
        .filter(|kind| !matches!(kind.spec().support, HarnessSupport::Unsupported))
        .collect()
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
    fn claude_doctor_reports_project_and_history_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let old_project = ctx
            .claude_base()
            .join("projects")
            .join(claude_encode_cc_port(&old));
        write_file(&old_project.join("session-a.jsonl"), "{}\n");
        write_file(
            &ctx.claude_base().join("history.jsonl"),
            &format!(
                "{{\"project\":\"{}\",\"sessionId\":\"session-a\",\"timestamp\":1,\"display\":\"x\"}}\n",
                old.display()
            ),
        );

        let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
        let claude = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Claude)
            .unwrap();

        assert_eq!(claude.sessions_found, 1);
        assert_eq!(claude.path_references_found, 1);
        assert!(claude
            .operations
            .iter()
            .any(|op| op.action == "rename_project_dir" && !op.apply_ready));
        assert!(claude
            .operations
            .iter()
            .any(|op| op.action == "rewrite_history_paths" && !op.apply_ready));
        assert!(!report.has_blockers());
    }

    #[test]
    fn doctor_blocks_nested_destinations_and_existing_claude_target() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("repo");
        let new = old.join("nested");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let new_project = ctx
            .claude_base()
            .join("projects")
            .join(claude_encode_cc_port(&new));
        fs::create_dir_all(new_project).unwrap();

        let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
        assert!(report.has_blockers());
        assert!(report
            .risks
            .iter()
            .any(|risk| risk.message.contains("nested inside source")));
        assert!(report
            .risks
            .iter()
            .any(|risk| risk.message.contains("destination project folder")));
    }

    #[test]
    fn claude_project_keys_ignore_trailing_slashes() {
        let path = Path::new("/home/nuck/holoq/repo-os/claude-babel/");
        assert_eq!(
            claude_encode_cc_port(path),
            "-home-nuck-holoq-repo-os-claude-babel"
        );
        assert_eq!(
            claude_encode_ccmv(path),
            "-home-nuck-holoq-repo-os-claude-babel"
        );
    }

    #[test]
    fn codex_uses_native_session_identity_and_project_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("project");
        let new = home.join("project-renamed");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        write_file(
            &ctx.codex_sessions()
                .join("2026/04/29/rollout-2026-04-29T12-00-00-codex-session.jsonl"),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n",
                old.display()
            ),
        );
        write_file(
            &ctx.codex_sessions()
                .join("2026/04/29/rollout-2026-04-29T12-00-00-unrelated.jsonl"),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"unrelated\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"{}\"}}}}\n",
                home.join("other").display(),
                old.display()
            ),
        );
        write_file(
            &ctx.codex_base().join("history.jsonl"),
            &format!(
                "{{\"session_id\":\"codex-session\",\"text\":\"{}\"}}\n",
                old.display()
            ),
        );
        write_file(
            &ctx.codex_base().join("config.toml"),
            &format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                old.display()
            ),
        );
        write_file(
            &ctx.codex_shell_snapshots().join("codex-session.1.sh"),
            &format!("cd {}\n", old.display()),
        );
        write_file(
            &ctx.gemini_tmp().join("hash/chats/session.json"),
            &format!("{{\"project\":\"{}\"}}\n", old.display()),
        );

        let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
        let codex = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Codex)
            .unwrap();
        assert_eq!(codex.sessions_found, 1);
        assert_eq!(codex.path_references_found, 5);
        assert!(matches!(codex.readiness, AdapterReadiness::DoctorOnly));
        assert!(codex.operations.iter().all(|op| !op.apply_ready));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "rewrite_session_meta_cwd"));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "rewrite_project_config_keys"));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "preserve_session_shell_snapshots"));

        let gemini = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Gemini)
            .unwrap();
        assert_eq!(gemini.path_references_found, 1);
    }
}
