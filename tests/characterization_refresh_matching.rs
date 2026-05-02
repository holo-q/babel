//! Characterization tests for daemon refresh/matching handoff points.
//!
//! These tests pin the deterministic parts of the current refresh pipeline before
//! extracting snapshot/diff/apply and a matching coordinator. Live kitty I/O stays
//! outside this file; the protected seam is the state snapshot passed to matching
//! and the state mutation applied after fingerprint I/O returns.

use std::path::{Path, PathBuf};

use claude_babel::daemon::BabelState;
use claude_babel::fingerprint::{MatchConfidence, SessionFingerprint};
use claude_babel::kitty::PaneAddr;
use claude_babel::utility::agent_discovery::AgentPane;
use claude_babel::AgentKind;

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
