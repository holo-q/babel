//! Characterization tests for daemon refresh/matching handoff points.
//!
//! These tests pin the deterministic parts of the current refresh pipeline before
//! extracting snapshot/diff/apply and a matching coordinator. Live kitty I/O stays
//! outside this file; the protected seam is the state snapshot passed to matching
//! and the state mutation applied after fingerprint I/O returns.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use babel::daemon::{BabelState, PaneIdResolution, SocketStatus, TerminalInfo};
use babel::fingerprint::{MatchConfidence, SessionFingerprint};
use babel::model::PaneAddr;
use babel::service::matching::{
    coordinate_matches, ClaimKind, FingerprintMatch, MatchCandidate, MatchOutcome,
};
use babel::utility::agent_discovery::AgentPane;
use babel::AgentKind;
use serde_json::json;

#[derive(Debug, PartialEq, Eq)]
struct MatchingSnapshot {
    candidates: Vec<(String, u64, String)>,
    claimed_sessions: Vec<String>,
}

fn capture_matching_snapshot(state: &BabelState) -> MatchingSnapshot {
    let mut candidates: Vec<_> = state
        .get_panes_needing_fingerprints()
        .into_iter()
        .map(|(addr, cwd)| (addr.socket, addr.id, cwd.display().to_string()))
        .collect();
    candidates.sort();

    let mut claimed_sessions: Vec<_> = state.get_claimed_sessions().into_iter().collect();
    claimed_sessions.sort();

    MatchingSnapshot {
        candidates,
        claimed_sessions,
    }
}

fn insert_pane(
    state: &mut BabelState,
    socket: &str,
    id: u64,
    cwd: &str,
    session_id: Option<&str>,
) -> PaneAddr {
    let addr = PaneAddr::new(socket, id);
    state.panes.insert(
        addr.clone(),
        AgentPane {
            addr: addr.clone(),
            title: format!("pane-{id}"),
            session_id: session_id.map(str::to_owned),
            session_info: None,
            cwd: PathBuf::from(cwd),
            is_focused: false,
            os_window_id: id + 1000,
            platform_window_id: id + 2000,
            workspace: Some(0),
            agent_kind: AgentKind::Claude,
            screen: None,
            activity_state: None,
            hook_state: None,
            fingerprint: None,
            match_confidence: None,
        },
    );
    addr
}

#[test]
fn matching_snapshot_keeps_only_unmatched_panes_and_claimed_sessions() {
    let mut state = BabelState::default();
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-alpha",
        7,
        "/work/alpha",
        None,
    );
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-beta",
        7,
        "/work/beta",
        Some("session-beta"),
    );
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-gamma",
        9,
        "/work/gamma",
        None,
    );

    let snapshot = capture_matching_snapshot(&state);

    assert_eq!(
        snapshot,
        MatchingSnapshot {
            candidates: vec![
                (
                    "unix:/run/user/1000/kitty.sock-alpha".to_string(),
                    7,
                    "/work/alpha".to_string(),
                ),
                (
                    "unix:/run/user/1000/kitty.sock-gamma".to_string(),
                    9,
                    "/work/gamma".to_string(),
                ),
            ],
            claimed_sessions: vec!["session-beta".to_string()],
        }
    );
}

#[test]
fn apply_fingerprint_result_updates_only_the_addressed_pane_and_cache() {
    let mut state = BabelState::default();
    let target = insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-alpha",
        7,
        "/work/alpha",
        None,
    );
    let neighbor = insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-beta",
        7,
        "/work/beta",
        None,
    );
    let fingerprint = SessionFingerprint {
        first_prompt: Some("protect the matching seam".to_string()),
        recent_prompts: vec!["protect the matching seam".to_string()],
        tool_sequence: vec!["Read".to_string(), "Edit".to_string()],
        cwd: Some(PathBuf::from("/work/alpha")),
        timestamp: Some("2026-05-01T00:00:00Z".to_string()),
        session_id: Some("session-alpha".to_string()),
    };

    state.apply_fingerprint_result(
        &target,
        "session-alpha".to_string(),
        MatchConfidence::High,
        fingerprint,
    );

    let target_pane = state
        .panes
        .get(&target)
        .expect("target pane remains tracked");
    assert_eq!(target_pane.session_id.as_deref(), Some("session-alpha"));
    assert_eq!(target_pane.match_confidence, Some(MatchConfidence::High));
    assert_eq!(
        target_pane
            .fingerprint
            .as_ref()
            .and_then(|fp| fp.first_prompt.as_deref()),
        Some("protect the matching seam")
    );

    let cached = state
        .pane_fingerprints
        .get(&target)
        .expect("fingerprint is cached under full pane address");
    assert_eq!(cached.cwd.as_deref(), Some(Path::new("/work/alpha")));

    let neighbor_pane = state
        .panes
        .get(&neighbor)
        .expect("neighbor pane remains tracked");
    assert_eq!(neighbor_pane.session_id, None);
    assert_eq!(neighbor_pane.match_confidence, None);
    assert!(!state.pane_fingerprints.contains_key(&neighbor));
}

#[test]
fn resolve_legacy_pane_id_distinguishes_unique_ambiguous_and_missing() {
    // Multi-kitty collisions are real: the same kitty pane id can be live on
    // two sockets simultaneously. The legacy u64 resolver must refuse to
    // pick a winner so an IPC request for "pane 7" cannot silently land on
    // the wrong socket.
    let mut state = BabelState::default();
    let alpha = insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-alpha",
        7,
        "/work/alpha",
        None,
    );
    let beta = insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-beta",
        7,
        "/work/beta",
        None,
    );
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-gamma",
        9,
        "/work/gamma",
        None,
    );

    match state.resolve_legacy_pane_id(9) {
        PaneIdResolution::Found(addr) => {
            assert_eq!(addr.socket, "unix:/run/user/1000/kitty.sock-gamma");
            assert_eq!(addr.id, 9);
        }
        other => panic!("expected unique resolution for id 9, got {:?}", other),
    }

    match state.resolve_legacy_pane_id(7) {
        PaneIdResolution::Ambiguous(mut candidates) => {
            candidates.sort_by(|a, b| a.socket.cmp(&b.socket));
            assert_eq!(candidates, vec![alpha.clone(), beta.clone()]);
        }
        other => panic!("expected ambiguous resolution for id 7, got {:?}", other),
    }

    assert!(matches!(
        state.resolve_legacy_pane_id(99),
        PaneIdResolution::NotFound
    ));
}

#[test]
fn legacy_id_shims_refuse_ambiguous_lookups() {
    // The whole point of the address-first refactor is that the legacy
    // u64 → pane shims used by IPC request handlers must NOT silently pick
    // a winner when the id is ambiguous. They return None, log the
    // candidates, and let the IPC layer surface a clean "not found".
    let mut state = BabelState::default();
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-alpha",
        42,
        "/work/alpha",
        None,
    );
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-beta",
        42,
        "/work/beta",
        None,
    );

    assert!(state.find_pane_by_id(42).is_none());
    assert!(state.find_pane_by_id_mut(42).is_none());
    assert!(state.find_addr_by_id(42).is_none());
    assert!(state.get_activity_state(42).is_none());

    // Sanity: the address-first APIs still work for either side of the
    // collision, because the address itself is unambiguous.
    let alpha = PaneAddr::new("unix:/run/user/1000/kitty.sock-alpha", 42);
    assert!(state.find_pane(&alpha).is_some());
    assert!(state.find_pane_mut(&alpha).is_some());
}

#[test]
fn legacy_id_shims_resolve_when_unique() {
    let mut state = BabelState::default();
    insert_pane(
        &mut state,
        "unix:/run/user/1000/kitty.sock-alpha",
        7,
        "/work/alpha",
        Some("session-alpha"),
    );

    let pane = state
        .find_pane_by_id(7)
        .expect("unique legacy id resolves cleanly");
    assert_eq!(pane.session_id.as_deref(), Some("session-alpha"));

    let addr = state
        .find_addr_by_id(7)
        .expect("unique legacy id resolves to address");
    assert_eq!(addr.id, 7);
    assert_eq!(addr.socket, "unix:/run/user/1000/kitty.sock-alpha");
}

#[test]
fn terminal_info_and_socket_status_keep_compat_imports_and_wire_shape() {
    // Wave 5 moves the state DTOs into `babel::service::state` while keeping
    // `babel::daemon::{TerminalInfo, SocketStatus}` re-exports for downstream
    // CLIs and panel widgets that already import them. This test pins both
    // ends of that contract: the `daemon::*` path stays usable, and the
    // serde shape — including the legacy `is_claude` alias on `is_agent` —
    // survives the move so existing JSON snapshots keep decoding.
    let info = TerminalInfo {
        addr: PaneAddr::new("unix:/run/user/1000/kitty.sock-alpha", 7),
        title: "fish".to_string(),
        cwd: PathBuf::from("/work/alpha"),
        foreground_command: Some("fish".to_string()),
        workspace: Some(2),
        is_agent: true,
        is_focused: false,
        platform_window_id: 4242,
    };
    assert_eq!(info.id(), 7);

    let info_json = serde_json::to_value(&info).unwrap();
    assert_eq!(
        info_json,
        json!({
            "addr": { "socket": "unix:/run/user/1000/kitty.sock-alpha", "id": 7 },
            "title": "fish",
            "cwd": "/work/alpha",
            "foreground_command": "fish",
            "workspace": 2,
            "is_agent": true,
            "is_focused": false,
            "platform_window_id": 4242,
        })
    );

    // Legacy snapshots written before the agent rename still decode via the
    // serde alias. Drop this and panels with cached state files break on
    // upgrade.
    let legacy: TerminalInfo = serde_json::from_value(json!({
        "addr": { "socket": "unix:/run/user/1000/kitty.sock-alpha", "id": 7 },
        "title": "claude",
        "cwd": "/work/alpha",
        "workspace": null,
        "is_claude": true,
        "is_focused": false,
        "platform_window_id": 4242,
    }))
    .unwrap();
    assert!(legacy.is_agent);
    assert_eq!(legacy.foreground_command, None);

    let status = SocketStatus {
        is_current: true,
        is_responsive: true,
        pane_count: 3,
        last_error: None,
    };
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        json!({
            "is_current": true,
            "is_responsive": true,
            "pane_count": 3,
            "last_error": null,
        })
    );
}

#[test]
fn paint_pane_id_includes_socket_pid_to_disambiguate_collisions() {
    // Paint stream consumers (richmon-babel, richspace-babel) use the paint
    // id as an opaque map key. With bare `k{id}`, two live panes with the
    // same kitty id on different sockets would collapse onto a single dot.
    // The address-derived form keeps them distinct.
    let alpha = PaneAddr::new("unix:/run/user/1000/kitty.sock-alpha", 7);
    let beta = PaneAddr::new("unix:/run/user/1000/kitty.sock-beta", 7);

    let alpha_id = BabelState::paint_pane_id(&alpha);
    let beta_id = BabelState::paint_pane_id(&beta);

    assert_ne!(alpha_id, beta_id);
    assert!(alpha_id.contains("alpha"));
    assert!(beta_id.contains("beta"));
}

// ─────────────────────────────────────────────────────────────────────────
// Matching coordinator characterization
//
// Wave 4 introduced `babel::service::matching` as the single arbitration
// point: callers (daemon refresh, IPC) now hand a batch of `MatchCandidate`s
// plus the externally-claimed session set to `coordinate_matches` and apply
// the returned `MatchDecision`s. The four tests below pin the contract that
// downstream code now depends on:
//
//   1. session ids already held — by panes outside the batch (the
//      `already_claimed` set) or by an in-batch pane via `current_session_id`
//      — block any later same-batch claim against the same session,
//   2. when two candidate panes target the same free session in the same
//      batch, sort order produces a deterministic winner and the loser is
//      surfaced as a `DuplicateClaim` (no silent drop),
//   3. when both signals are free, title beats fingerprint (cheaper signal
//      and the one humans see in the kitty tab),
//   4. a candidate with no evidence yields `NoMatch` and never a synthetic
//      session id — the daemon must not invent identity downstream.

fn matching_addr(socket: &str, id: u64) -> PaneAddr {
    PaneAddr::new(format!("unix:/run/user/1000/kitty.sock-{socket}"), id)
}

fn matching_fp(session_id: &str, confidence: MatchConfidence) -> FingerprintMatch {
    FingerprintMatch {
        session_id: session_id.to_string(),
        confidence,
        fingerprint: SessionFingerprint::default(),
    }
}

#[test]
fn refresh_matching_existing_sessions_block_later_batch_claims() {
    let already: HashSet<String> = ["S_EXTERNAL".to_string()].into_iter().collect();

    // Three candidates in one batch:
    // - id=1 already owns "S_OWNED" → AlreadyMatched, joins batch claim set,
    // - id=2 tries to title-claim "S_OWNED" → blocked by the in-batch claim,
    // - id=3 tries to title-claim "S_EXTERNAL" → blocked by `already_claimed`.
    let candidates = vec![
        MatchCandidate::new(matching_addr("a", 1)).with_current_session(Some("S_OWNED".into())),
        MatchCandidate::new(matching_addr("a", 2)).with_title_match(Some("S_OWNED".into())),
        MatchCandidate::new(matching_addr("a", 3)).with_title_match(Some("S_EXTERNAL".into())),
    ];

    let decisions = coordinate_matches(candidates, &already);

    assert!(matches!(
        &decisions[0].outcome,
        MatchOutcome::AlreadyMatched { session_id } if session_id == "S_OWNED"
    ));
    assert!(matches!(
        &decisions[1].outcome,
        MatchOutcome::DuplicateClaim { session_id, kind: ClaimKind::Title }
            if session_id == "S_OWNED"
    ));
    assert!(matches!(
        &decisions[2].outcome,
        MatchOutcome::DuplicateClaim { session_id, kind: ClaimKind::Title }
            if session_id == "S_EXTERNAL"
    ));
}

#[test]
fn refresh_matching_same_batch_duplicate_picks_deterministic_winner() {
    // Two panes both want session "S" via title in the same batch. Sort
    // order — (socket, id) — puts ("a", 1) before ("a", 2) regardless of
    // input order, so the first wins and the second surfaces a
    // DuplicateClaim. This is the silent-double-claim bug Wave 4 fixes.
    let candidates = vec![
        MatchCandidate::new(matching_addr("a", 2)).with_title_match(Some("S".into())),
        MatchCandidate::new(matching_addr("a", 1)).with_title_match(Some("S".into())),
    ];

    let decisions = coordinate_matches(candidates, &HashSet::new());

    assert_eq!(decisions[0].addr, matching_addr("a", 1));
    assert!(matches!(
        &decisions[0].outcome,
        MatchOutcome::TitleMatched { session_id } if session_id == "S"
    ));
    assert_eq!(decisions[1].addr, matching_addr("a", 2));
    assert!(matches!(
        &decisions[1].outcome,
        MatchOutcome::DuplicateClaim { session_id, kind: ClaimKind::Title }
            if session_id == "S"
    ));
}

#[test]
fn refresh_matching_title_beats_fingerprint_when_both_free() {
    let candidates = vec![MatchCandidate::new(matching_addr("a", 1))
        .with_title_match(Some("S_TITLE".into()))
        .with_fingerprint_match(Some(matching_fp("S_FP", MatchConfidence::High)))];

    let decisions = coordinate_matches(candidates, &HashSet::new());

    assert!(matches!(
        &decisions[0].outcome,
        MatchOutcome::TitleMatched { session_id } if session_id == "S_TITLE"
    ));
}

#[test]
fn refresh_matching_unmatched_pane_yields_no_match_with_no_synthetic_id() {
    // A candidate with neither title nor fingerprint evidence must yield
    // NoMatch. The variant carries no payload by design: the caller is
    // forbidden from synthesising a session id for an unmatched pane.
    let candidates = vec![MatchCandidate::new(matching_addr("a", 1))];

    let decisions = coordinate_matches(candidates, &HashSet::new());

    assert_eq!(decisions.len(), 1);
    assert!(matches!(decisions[0].outcome, MatchOutcome::NoMatch));
}
