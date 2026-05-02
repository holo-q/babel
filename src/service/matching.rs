//! Matching coordinator.
//!
//! Title-derived claims, fingerprint claims, and "this pane already has a
//! session" facts arrive scattered around the daemon's refresh and IPC
//! paths. Each path independently decides whether to assign a session id
//! to a pane, which historically led to two panes silently grabbing the
//! same session inside a single poll tick.
//!
//! This module is the single policy point. The daemon does the I/O
//! (scrollback, summary index lookup, fingerprint match) and then hands a
//! batch of [`MatchCandidate`]s plus the current claim set to
//! [`coordinate_matches`]. The coordinator returns one
//! [`MatchDecision`] per pane, with the same-batch arbitration baked in:
//! sort by [`PaneAddr`] socket then id, and the first claimant of any
//! given session wins. Later candidates that wanted the same session are
//! flagged with [`MatchOutcome::DuplicateClaim`] so the daemon can log
//! the loss instead of silently dropping it.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::fingerprint::{MatchConfidence, SessionFingerprint};
use crate::kitty::PaneAddr;

/// Fingerprint match evidence produced upstream.
///
/// The expensive scrollback fetch and per-session scoring lives in the
/// daemon. The coordinator only needs the winning session id, the
/// confidence at which it won, and the fingerprint snapshot so the
/// daemon can cache it on the pane after the decision is applied.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintMatch {
    pub session_id: String,
    pub confidence: MatchConfidence,
    pub fingerprint: SessionFingerprint,
}

/// Per-pane matching evidence collected for a single coordination batch.
///
/// `current_session_id` is the pane's existing claim, if any — short-circuits
/// matching. `title_match` is the session id resolved from the kitty title
/// via the summary index (cheap, in-memory). `fingerprint_match` is the
/// fingerprint resolution result (expensive scrollback I/O). Either or both
/// may be absent.
#[derive(Debug, Clone)]
pub struct MatchCandidate {
    pub addr: PaneAddr,
    pub current_session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub title_match: Option<String>,
    pub fingerprint_match: Option<FingerprintMatch>,
}

impl MatchCandidate {
    pub fn new(addr: PaneAddr) -> Self {
        Self {
            addr,
            current_session_id: None,
            cwd: None,
            title_match: None,
            fingerprint_match: None,
        }
    }

    pub fn with_current_session(mut self, session_id: Option<String>) -> Self {
        self.current_session_id = session_id;
        self
    }

    pub fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }

    pub fn with_title_match(mut self, session_id: Option<String>) -> Self {
        self.title_match = session_id;
        self
    }

    pub fn with_fingerprint_match(mut self, fp: Option<FingerprintMatch>) -> Self {
        self.fingerprint_match = fp;
        self
    }
}

/// Why a particular evidence source lost the race for a session id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    Title,
    Fingerprint,
}

/// Per-pane decision emitted by the coordinator.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    /// Pane already had a session id; nothing to apply.
    AlreadyMatched { session_id: String },
    /// Title-derived summary mapped to a session that no pane outside
    /// this batch and no earlier candidate inside it had claimed.
    TitleMatched { session_id: String },
    /// Fingerprint resolved to a session that no pane outside this
    /// batch and no earlier candidate inside it had claimed.
    FingerprintMatched {
        session_id: String,
        confidence: MatchConfidence,
        fingerprint: SessionFingerprint,
    },
    /// The candidate's only available claim targeted a session that an
    /// existing pane (or an earlier same-batch decision) had already
    /// taken. The daemon should not assign this pane and should log the
    /// duplicate so the conflict is visible.
    DuplicateClaim { session_id: String, kind: ClaimKind },
    /// Neither title nor fingerprint produced a usable claim.
    NoMatch,
}

/// One coordinated decision per input pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchDecision {
    pub addr: PaneAddr,
    pub outcome: MatchOutcome,
}

/// Resolve a batch of candidates into deterministic, conflict-free
/// decisions.
///
/// Inputs:
/// - `candidates`: every pane the caller wants resolved this tick. Order
///   does not matter; the coordinator sorts by `(socket, id)` so the
///   same input always yields the same output.
/// - `already_claimed`: session ids currently held by panes the caller
///   is *not* re-considering this tick (e.g. panes outside the matching
///   set). These are off-limits for any new claim.
///
/// Resolution order per candidate:
/// 1. If `current_session_id` is set, the pane is `AlreadyMatched`.
///    Its session id joins the in-batch claim set so other candidates
///    that target it become `DuplicateClaim`.
/// 2. Otherwise, try the title match. If its session is free, claim it
///    as `TitleMatched`.
/// 3. Otherwise, try the fingerprint match. If its session is free,
///    claim it as `FingerprintMatched`.
/// 4. Otherwise, if any attempted claim collided with an already-taken
///    session, surface a `DuplicateClaim` (title preferred over
///    fingerprint when both collided). If neither evidence was present,
///    surface `NoMatch`.
pub fn coordinate_matches(
    candidates: Vec<MatchCandidate>,
    already_claimed: &HashSet<String>,
) -> Vec<MatchDecision> {
    let mut sorted = candidates;
    sorted.sort_by(|a, b| {
        a.addr
            .socket
            .cmp(&b.addr.socket)
            .then_with(|| a.addr.id.cmp(&b.addr.id))
    });

    let mut batch_claimed: HashSet<String> = HashSet::new();
    let mut decisions: Vec<MatchDecision> = Vec::with_capacity(sorted.len());

    for candidate in sorted {
        let outcome = decide_one(&candidate, already_claimed, &batch_claimed);
        match &outcome {
            MatchOutcome::AlreadyMatched { session_id }
            | MatchOutcome::TitleMatched { session_id }
            | MatchOutcome::FingerprintMatched { session_id, .. } => {
                batch_claimed.insert(session_id.clone());
            }
            MatchOutcome::DuplicateClaim { .. } | MatchOutcome::NoMatch => {}
        }
        decisions.push(MatchDecision {
            addr: candidate.addr,
            outcome,
        });
    }

    decisions
}

fn decide_one(
    candidate: &MatchCandidate,
    already_claimed: &HashSet<String>,
    batch_claimed: &HashSet<String>,
) -> MatchOutcome {
    if let Some(session_id) = candidate.current_session_id.clone() {
        return MatchOutcome::AlreadyMatched { session_id };
    }

    let is_taken = |sid: &str| already_claimed.contains(sid) || batch_claimed.contains(sid);

    if let Some(title_sid) = &candidate.title_match {
        if !is_taken(title_sid) {
            return MatchOutcome::TitleMatched {
                session_id: title_sid.clone(),
            };
        }
    }

    if let Some(fp) = &candidate.fingerprint_match {
        if !is_taken(&fp.session_id) {
            return MatchOutcome::FingerprintMatched {
                session_id: fp.session_id.clone(),
                confidence: fp.confidence,
                fingerprint: fp.fingerprint.clone(),
            };
        }
    }

    if let Some(title_sid) = &candidate.title_match {
        if is_taken(title_sid) {
            return MatchOutcome::DuplicateClaim {
                session_id: title_sid.clone(),
                kind: ClaimKind::Title,
            };
        }
    }
    if let Some(fp) = &candidate.fingerprint_match {
        if is_taken(&fp.session_id) {
            return MatchOutcome::DuplicateClaim {
                session_id: fp.session_id.clone(),
                kind: ClaimKind::Fingerprint,
            };
        }
    }

    MatchOutcome::NoMatch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(socket: &str, id: u64) -> PaneAddr {
        PaneAddr::new(format!("unix:/run/user/1000/kitty.sock-{socket}"), id)
    }

    fn fp_match(session_id: &str, confidence: MatchConfidence) -> FingerprintMatch {
        FingerprintMatch {
            session_id: session_id.to_string(),
            confidence,
            fingerprint: SessionFingerprint::default(),
        }
    }

    fn assert_already(outcome: &MatchOutcome, expected: &str) {
        match outcome {
            MatchOutcome::AlreadyMatched { session_id } => {
                assert_eq!(session_id, expected);
            }
            other => panic!("expected AlreadyMatched({expected}), got {other:?}"),
        }
    }

    fn assert_title(outcome: &MatchOutcome, expected: &str) {
        match outcome {
            MatchOutcome::TitleMatched { session_id } => {
                assert_eq!(session_id, expected);
            }
            other => panic!("expected TitleMatched({expected}), got {other:?}"),
        }
    }

    fn assert_fingerprint(outcome: &MatchOutcome, expected: &str, conf: MatchConfidence) {
        match outcome {
            MatchOutcome::FingerprintMatched {
                session_id,
                confidence,
                ..
            } => {
                assert_eq!(session_id, expected);
                assert_eq!(*confidence, conf);
            }
            other => panic!("expected FingerprintMatched({expected}, {conf:?}), got {other:?}"),
        }
    }

    fn assert_duplicate(outcome: &MatchOutcome, expected: &str, kind: ClaimKind) {
        match outcome {
            MatchOutcome::DuplicateClaim {
                session_id,
                kind: k,
            } => {
                assert_eq!(session_id, expected);
                assert_eq!(*k, kind);
            }
            other => panic!("expected DuplicateClaim({expected}, {kind:?}), got {other:?}"),
        }
    }

    fn assert_no_match(outcome: &MatchOutcome) {
        assert!(
            matches!(outcome, MatchOutcome::NoMatch),
            "expected NoMatch, got {outcome:?}"
        );
    }

    #[test]
    fn pane_with_existing_session_is_already_matched() {
        let candidates = vec![MatchCandidate::new(addr("a", 1))
            .with_current_session(Some("S1".into()))
            .with_title_match(Some("S2".into()))];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_already(&decisions[0].outcome, "S1");
    }

    #[test]
    fn title_match_claims_unclaimed_session() {
        let candidates =
            vec![MatchCandidate::new(addr("a", 1)).with_title_match(Some("S1".into()))];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_title(&decisions[0].outcome, "S1");
    }

    #[test]
    fn fingerprint_used_when_title_absent() {
        let candidates = vec![MatchCandidate::new(addr("a", 1))
            .with_fingerprint_match(Some(fp_match("S1", MatchConfidence::High)))];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_fingerprint(&decisions[0].outcome, "S1", MatchConfidence::High);
    }

    #[test]
    fn title_preferred_over_fingerprint_when_both_present() {
        let candidates = vec![MatchCandidate::new(addr("a", 1))
            .with_title_match(Some("S_TITLE".into()))
            .with_fingerprint_match(Some(fp_match("S_FP", MatchConfidence::High)))];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_title(&decisions[0].outcome, "S_TITLE");
    }

    #[test]
    fn taken_title_falls_through_to_fingerprint() {
        let mut already = HashSet::new();
        already.insert("S_TITLE".into());

        let candidates = vec![MatchCandidate::new(addr("a", 1))
            .with_title_match(Some("S_TITLE".into()))
            .with_fingerprint_match(Some(fp_match("S_FP", MatchConfidence::Medium)))];

        let decisions = coordinate_matches(candidates, &already);

        assert_fingerprint(&decisions[0].outcome, "S_FP", MatchConfidence::Medium);
    }

    #[test]
    fn external_claim_blocks_title_match() {
        let mut already = HashSet::new();
        already.insert("S1".into());

        let candidates =
            vec![MatchCandidate::new(addr("a", 1)).with_title_match(Some("S1".into()))];

        let decisions = coordinate_matches(candidates, &already);

        assert_duplicate(&decisions[0].outcome, "S1", ClaimKind::Title);
    }

    // Two panes both want session S via title in the same batch. Sort
    // order puts (socket="a", id=1) before (socket="a", id=2), so the
    // first wins and the second must surface DuplicateClaim — that is
    // the bug Wave 4 exists to fix.
    #[test]
    fn same_batch_title_collision_resolves_deterministically() {
        let candidates = vec![
            MatchCandidate::new(addr("a", 2)).with_title_match(Some("S".into())),
            MatchCandidate::new(addr("a", 1)).with_title_match(Some("S".into())),
        ];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        // After sort, addr("a", 1) is first.
        assert_eq!(decisions[0].addr, addr("a", 1));
        assert_title(&decisions[0].outcome, "S");
        assert_eq!(decisions[1].addr, addr("a", 2));
        assert_duplicate(&decisions[1].outcome, "S", ClaimKind::Title);
    }

    #[test]
    fn same_batch_fingerprint_collision_resolves_deterministically() {
        let candidates = vec![
            MatchCandidate::new(addr("a", 2))
                .with_fingerprint_match(Some(fp_match("S", MatchConfidence::High))),
            MatchCandidate::new(addr("a", 1))
                .with_fingerprint_match(Some(fp_match("S", MatchConfidence::High))),
        ];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_eq!(decisions[0].addr, addr("a", 1));
        assert_fingerprint(&decisions[0].outcome, "S", MatchConfidence::High);
        assert_duplicate(&decisions[1].outcome, "S", ClaimKind::Fingerprint);
    }

    // Sort key is (socket, id), so a numerically larger socket string
    // can sort before a smaller one. Pin lexicographic ordering so
    // future code can rely on it for replay.
    #[test]
    fn sort_order_is_socket_then_id_lexicographic() {
        let candidates = vec![
            MatchCandidate::new(addr("zzz", 1)).with_title_match(Some("S1".into())),
            MatchCandidate::new(addr("aaa", 999)).with_title_match(Some("S2".into())),
        ];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert!(decisions[0].addr.socket.contains("aaa"));
        assert!(decisions[1].addr.socket.contains("zzz"));
    }

    // An AlreadyMatched pane must add its session to the batch claim
    // set, otherwise a later candidate could attempt to claim the same
    // session via fingerprint and silently double-claim it.
    #[test]
    fn already_matched_blocks_later_batch_claims() {
        let candidates = vec![
            MatchCandidate::new(addr("a", 1)).with_current_session(Some("S".into())),
            MatchCandidate::new(addr("a", 2))
                .with_fingerprint_match(Some(fp_match("S", MatchConfidence::High))),
        ];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_already(&decisions[0].outcome, "S");
        assert_duplicate(&decisions[1].outcome, "S", ClaimKind::Fingerprint);
    }

    #[test]
    fn no_evidence_yields_no_match() {
        let candidates = vec![MatchCandidate::new(addr("a", 1))];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_no_match(&decisions[0].outcome);
    }

    // When both title and fingerprint target taken sessions and there
    // is no fallback, prefer to surface the title duplicate. Title
    // match is the cheaper signal and the one humans see in the kitty
    // tab, so flagging it first makes the loss legible in logs.
    #[test]
    fn duplicate_claim_prefers_title_kind_when_both_collide() {
        let mut already = HashSet::new();
        already.insert("S_TITLE".into());
        already.insert("S_FP".into());

        let candidates = vec![MatchCandidate::new(addr("a", 1))
            .with_title_match(Some("S_TITLE".into()))
            .with_fingerprint_match(Some(fp_match("S_FP", MatchConfidence::High)))];

        let decisions = coordinate_matches(candidates, &already);

        assert_duplicate(&decisions[0].outcome, "S_TITLE", ClaimKind::Title);
    }

    // Mixed batch: pane 1 already matched, pane 2 title-claims a free
    // session, pane 3 fingerprint-claims the session pane 2 just took
    // (so it surfaces a DuplicateClaim), pane 4 has no evidence at all.
    #[test]
    fn mixed_batch_resolves_each_outcome_kind() {
        let candidates = vec![
            MatchCandidate::new(addr("a", 1)).with_current_session(Some("S_OLD".into())),
            MatchCandidate::new(addr("a", 2)).with_title_match(Some("S_T".into())),
            MatchCandidate::new(addr("a", 3))
                .with_fingerprint_match(Some(fp_match("S_T", MatchConfidence::High))),
            MatchCandidate::new(addr("a", 4)),
        ];

        let decisions = coordinate_matches(candidates, &HashSet::new());

        assert_already(&decisions[0].outcome, "S_OLD");
        assert_title(&decisions[1].outcome, "S_T");
        assert_duplicate(&decisions[2].outcome, "S_T", ClaimKind::Fingerprint);
        assert_no_match(&decisions[3].outcome);
    }
}
