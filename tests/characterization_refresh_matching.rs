//! Characterization tests for daemon refresh/matching handoff points.
//!
//! These tests pin the deterministic parts of the current refresh pipeline before
//! extracting snapshot/diff/apply and a matching coordinator. Live kitty I/O stays
//! outside this file; the protected seam is the state snapshot passed to matching
//! and the state mutation applied after fingerprint I/O returns.

use std::path::{Path, PathBuf};

use babel::daemon::{BabelState, PaneIdResolution};
use babel::fingerprint::{MatchConfidence, SessionFingerprint};
use babel::model::PaneAddr;
use babel::utility::agent_discovery::AgentPane;
use babel::AgentKind;

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
    let mut state = BabelState::new();
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
    let mut state = BabelState::new();
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
    let mut state = BabelState::new();
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
    let mut state = BabelState::new();
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
    let mut state = BabelState::new();
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
