//! Activity reducer.
//!
//! The reducer is the single policy point for deciding which pane activity
//! observation wins. Hook events are direct harness truth while fresh;
//! scrollback is recovery/poll evidence; focus is a read-state signal, not a
//! synthetic work-state signal.

use crate::babel_storage::HookState;
use crate::model::{ActivitySource, PaneActivity};
use crate::ActivityState;

pub const DEFAULT_HOOK_FRESH_MS: i64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityObservation {
    pub state: ActivityState,
    pub source: ActivitySource,
    pub observed_at_ms: i64,
}

impl ActivityObservation {
    pub fn new(state: ActivityState, source: ActivitySource, observed_at_ms: i64) -> Self {
        Self {
            state,
            source,
            observed_at_ms,
        }
    }

    pub fn from_hook_state(state: HookState, observed_at_ms: i64) -> Self {
        Self::new(
            hook_state_activity(state),
            ActivitySource::Hook,
            observed_at_ms,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityReduction {
    pub activity: PaneActivity,
    pub accepted: bool,
    pub reason: ActivityDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityDecision {
    Initial,
    NewerHook,
    NewerObservation,
    IgnoredOlderObservation,
    IgnoredFreshHookPrecedence,
}

pub fn hook_state_activity(state: HookState) -> ActivityState {
    match state {
        HookState::Working => ActivityState::Thinking,
        HookState::ToolRunning => ActivityState::ToolUse,
        HookState::Idle => ActivityState::Idle,
    }
}

pub fn reduce_activity(
    current: Option<&PaneActivity>,
    observation: ActivityObservation,
) -> ActivityReduction {
    reduce_activity_with_hook_fresh_ms(current, observation, DEFAULT_HOOK_FRESH_MS)
}

pub fn reduce_activity_with_hook_fresh_ms(
    current: Option<&PaneActivity>,
    observation: ActivityObservation,
    hook_fresh_ms: i64,
) -> ActivityReduction {
    let Some(current) = current else {
        return ActivityReduction {
            activity: PaneActivity::new(
                observation.state,
                observation.source,
                observation.observed_at_ms,
                0,
            ),
            accepted: true,
            reason: ActivityDecision::Initial,
        };
    };

    if observation.observed_at_ms < current.observed_at_ms {
        return ActivityReduction {
            activity: current.clone(),
            accepted: false,
            reason: ActivityDecision::IgnoredOlderObservation,
        };
    }

    if current.source == ActivitySource::Hook
        && observation.source == ActivitySource::Scrollback
        && observation
            .observed_at_ms
            .saturating_sub(current.observed_at_ms)
            <= hook_fresh_ms
    {
        return ActivityReduction {
            activity: current.clone(),
            accepted: false,
            reason: ActivityDecision::IgnoredFreshHookPrecedence,
        };
    }

    let reason = if observation.source == ActivitySource::Hook {
        ActivityDecision::NewerHook
    } else {
        ActivityDecision::NewerObservation
    };

    ActivityReduction {
        activity: PaneActivity::new(
            observation.state,
            observation.source,
            observation.observed_at_ms,
            current.next_generation(),
        ),
        accepted: true,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_state_maps_to_activity_state() {
        assert_eq!(
            hook_state_activity(HookState::Working),
            ActivityState::Thinking
        );
        assert_eq!(
            hook_state_activity(HookState::ToolRunning),
            ActivityState::ToolUse
        );
        assert_eq!(hook_state_activity(HookState::Idle), ActivityState::Idle);
    }

    #[test]
    fn first_observation_initializes_activity() {
        let reduction = reduce_activity(
            None,
            ActivityObservation::new(ActivityState::Thinking, ActivitySource::Scrollback, 100),
        );

        assert!(reduction.accepted);
        assert_eq!(reduction.reason, ActivityDecision::Initial);
        assert_eq!(reduction.activity.generation, 0);
        assert_eq!(reduction.activity.source, ActivitySource::Scrollback);
    }

    #[test]
    fn fresh_hook_state_beats_scrollback_poll() {
        let current = PaneActivity::new(ActivityState::ToolUse, ActivitySource::Hook, 1_000, 4);
        let reduction = reduce_activity_with_hook_fresh_ms(
            Some(&current),
            ActivityObservation::new(ActivityState::Idle, ActivitySource::Scrollback, 1_500),
            1_000,
        );

        assert!(!reduction.accepted);
        assert_eq!(
            reduction.reason,
            ActivityDecision::IgnoredFreshHookPrecedence
        );
        assert_eq!(reduction.activity, current);
    }

    #[test]
    fn stale_hook_state_can_be_superseded_by_scrollback() {
        let current = PaneActivity::new(ActivityState::ToolUse, ActivitySource::Hook, 1_000, 4);
        let reduction = reduce_activity_with_hook_fresh_ms(
            Some(&current),
            ActivityObservation::new(ActivityState::Idle, ActivitySource::Scrollback, 2_001),
            1_000,
        );

        assert!(reduction.accepted);
        assert_eq!(reduction.reason, ActivityDecision::NewerObservation);
        assert_eq!(reduction.activity.state, ActivityState::Idle);
        assert_eq!(reduction.activity.source, ActivitySource::Scrollback);
        assert_eq!(reduction.activity.generation, 5);
    }

    #[test]
    fn newer_hook_observation_always_wins() {
        let current = PaneActivity::new(ActivityState::Idle, ActivitySource::Scrollback, 1_000, 4);
        let reduction = reduce_activity(
            Some(&current),
            ActivityObservation::from_hook_state(HookState::Working, 1_001),
        );

        assert!(reduction.accepted);
        assert_eq!(reduction.reason, ActivityDecision::NewerHook);
        assert_eq!(reduction.activity.state, ActivityState::Thinking);
        assert_eq!(reduction.activity.source, ActivitySource::Hook);
    }

    #[test]
    fn older_observations_are_ignored() {
        let current = PaneActivity::new(
            ActivityState::Thinking,
            ActivitySource::Scrollback,
            1_000,
            4,
        );
        let reduction = reduce_activity(
            Some(&current),
            ActivityObservation::new(ActivityState::Idle, ActivitySource::Hook, 999),
        );

        assert!(!reduction.accepted);
        assert_eq!(reduction.reason, ActivityDecision::IgnoredOlderObservation);
        assert_eq!(reduction.activity, current);
    }
}
