//! Refresh diffing.
//!
//! Kitty discovery is I/O. Applying a refresh is policy and effects. This
//! module owns the pure middle step: given old pane facts and new pane facts,
//! compute the structural delta that the daemon should apply. Keeping this
//! seam pure makes the refresh pipeline testable before the rest of the daemon
//! is split out of runtime orchestration.

use std::collections::{HashMap, HashSet};

use crate::kitty::PaneAddr;
use crate::utility::agent_discovery::AgentPane;

pub trait RefreshPane {
    fn workspace(&self) -> Option<i32>;
    fn is_focused(&self) -> bool;
}

impl RefreshPane for AgentPane {
    fn workspace(&self) -> Option<i32> {
        self.workspace
    }

    fn is_focused(&self) -> bool {
        self.is_focused
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMove {
    pub addr: PaneAddr,
    pub old_workspace: Option<i32>,
    pub new_workspace: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusChange {
    pub old: Option<PaneAddr>,
    pub new: Option<PaneAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentPaneDiff {
    pub added: Vec<PaneAddr>,
    pub removed: Vec<PaneAddr>,
    pub moved: Vec<WorkspaceMove>,
    pub focus: Option<FocusChange>,
}

pub fn diff_agent_panes<T: RefreshPane>(
    old: &HashMap<PaneAddr, T>,
    new: &HashMap<PaneAddr, T>,
) -> AgentPaneDiff {
    let old_addrs: HashSet<_> = old.keys().cloned().collect();
    let new_addrs: HashSet<_> = new.keys().cloned().collect();

    let mut added: Vec<_> = new_addrs.difference(&old_addrs).cloned().collect();
    let mut removed: Vec<_> = old_addrs.difference(&new_addrs).cloned().collect();
    sort_addrs(&mut added);
    sort_addrs(&mut removed);

    let mut moved: Vec<_> = old_addrs
        .intersection(&new_addrs)
        .filter_map(|addr| {
            let old_workspace = old.get(addr).and_then(RefreshPane::workspace);
            let new_workspace = new.get(addr).and_then(RefreshPane::workspace);
            (old_workspace != new_workspace).then(|| WorkspaceMove {
                addr: addr.clone(),
                old_workspace,
                new_workspace,
            })
        })
        .collect();
    moved.sort_by(|a, b| compare_addr(&a.addr, &b.addr));

    let old_focused = focused_addr(old);
    let new_focused = focused_addr(new);
    let focus = (old_focused != new_focused).then_some(FocusChange {
        old: old_focused,
        new: new_focused,
    });

    AgentPaneDiff {
        added,
        removed,
        moved,
        focus,
    }
}

fn focused_addr<T: RefreshPane>(panes: &HashMap<PaneAddr, T>) -> Option<PaneAddr> {
    let mut focused: Vec<_> = panes
        .iter()
        .filter_map(|(addr, pane)| pane.is_focused().then_some(addr.clone()))
        .collect();
    sort_addrs(&mut focused);
    focused.into_iter().next()
}

fn sort_addrs(addrs: &mut [PaneAddr]) {
    addrs.sort_by(compare_addr);
}

fn compare_addr(a: &PaneAddr, b: &PaneAddr) -> std::cmp::Ordering {
    a.socket.cmp(&b.socket).then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct TestPane {
        workspace: Option<i32>,
        focused: bool,
    }

    impl RefreshPane for TestPane {
        fn workspace(&self) -> Option<i32> {
            self.workspace
        }

        fn is_focused(&self) -> bool {
            self.focused
        }
    }

    fn addr(socket: &str, id: u64) -> PaneAddr {
        PaneAddr::new(format!("unix:/run/user/1000/kitty.sock-{socket}"), id)
    }

    fn pane(workspace: Option<i32>, focused: bool) -> TestPane {
        TestPane { workspace, focused }
    }

    #[test]
    fn diff_reports_added_removed_moved_and_focus_change() {
        let a = addr("111", 1);
        let b = addr("111", 2);
        let c = addr("222", 1);
        let d = addr("333", 1);

        let old = HashMap::from([
            (a.clone(), pane(Some(1), true)),
            (b.clone(), pane(Some(2), false)),
            (c.clone(), pane(Some(3), false)),
        ]);
        let new = HashMap::from([
            (a.clone(), pane(Some(1), false)),
            (b.clone(), pane(Some(4), true)),
            (d.clone(), pane(Some(5), false)),
        ]);

        let diff = diff_agent_panes(&old, &new);

        assert_eq!(diff.added, vec![d.clone()]);
        assert_eq!(diff.removed, vec![c]);
        assert_eq!(
            diff.moved,
            vec![WorkspaceMove {
                addr: b.clone(),
                old_workspace: Some(2),
                new_workspace: Some(4),
            }]
        );
        assert_eq!(
            diff.focus,
            Some(FocusChange {
                old: Some(a),
                new: Some(b),
            })
        );
    }

    #[test]
    fn same_structure_has_empty_diff() {
        let a = addr("111", 1);
        let old = HashMap::from([(a.clone(), pane(Some(1), true))]);
        let new = HashMap::from([(a, pane(Some(1), true))]);

        assert_eq!(diff_agent_panes(&old, &new), AgentPaneDiff::default());
    }
}
