use std::collections::{HashMap, HashSet};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowIdentity {
    pub hwnd: isize,
    pub process_id: u32,
    pub thread_id: u32,
}

impl WindowIdentity {
    pub fn capture(hwnd: HWND) -> Option<Self> {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return None;
        }

        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if thread_id == 0 {
            return None;
        }

        Some(Self {
            hwnd: hwnd.0 as isize,
            process_id,
            thread_id,
        })
    }

    pub fn hwnd(self) -> HWND {
        HWND(self.hwnd as _)
    }

    pub fn still_matches(self) -> bool {
        Self::capture(self.hwnd()) == Some(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderLifecycleState {
    Initializing,
    Active,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderRecord {
    pub tracking: WindowIdentity,
    pub border_hwnd: isize,
    pub state: BorderLifecycleState,
}

impl BorderRecord {
    pub fn border_hwnd(self) -> HWND {
        HWND(self.border_hwnd as _)
    }
}

#[derive(Debug, Default)]
pub struct BorderRegistry {
    entries: HashMap<isize, BorderRecord>,
}

impl BorderRegistry {
    pub fn insert(&mut self, record: BorderRecord) -> Option<BorderRecord> {
        self.entries.insert(record.tracking.hwnd, record)
    }

    pub fn get(&self, tracking: HWND) -> Option<&BorderRecord> {
        self.entries.get(&(tracking.0 as isize))
    }

    pub fn get_by_key(&self, tracking: isize) -> Option<&BorderRecord> {
        self.entries.get(&tracking)
    }

    pub fn get_border(&self, tracking: HWND) -> Option<HWND> {
        self.get(tracking).map(|record| record.border_hwnd())
    }

    pub fn set_state(&mut self, identity: WindowIdentity, state: BorderLifecycleState) -> bool {
        let Some(record) = self.entries.get_mut(&identity.hwnd) else {
            return false;
        };
        if record.tracking != identity {
            return false;
        }
        record.state = state;
        true
    }

    pub fn remove(&mut self, tracking: HWND) -> Option<BorderRecord> {
        self.entries.remove(&(tracking.0 as isize))
    }

    pub fn remove_by_key(&mut self, tracking: isize) -> Option<BorderRecord> {
        self.entries.remove(&tracking)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, tracking: HWND) -> bool {
        self.entries.contains_key(&(tracking.0 as isize))
    }

    pub fn records(&self) -> Vec<BorderRecord> {
        self.entries.values().copied().collect()
    }

    pub fn border_hwnds(&self) -> Vec<HWND> {
        self.entries
            .values()
            .map(|record| record.border_hwnd())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(hwnd: isize, process_id: u32, thread_id: u32) -> WindowIdentity {
        WindowIdentity {
            hwnd,
            process_id,
            thread_id,
        }
    }

    fn record(identity: WindowIdentity, border_hwnd: isize) -> BorderRecord {
        BorderRecord {
            tracking: identity,
            border_hwnd,
            state: BorderLifecycleState::Initializing,
        }
    }

    #[test]
    fn replacing_reused_hwnd_preserves_new_identity() {
        let old = identity(10, 100, 1000);
        let new = identity(10, 200, 2000);
        let mut registry = BorderRegistry::default();

        registry.insert(record(old, 11));
        assert_eq!(registry.insert(record(new, 12)).unwrap().tracking, old);

        assert_eq!(registry.get_by_key(10).unwrap().tracking, new);
        assert_eq!(registry.get_border(HWND(10 as _)), Some(HWND(12 as _)));
    }

    #[test]
    fn stale_identity_cannot_mark_reused_hwnd_active() {
        let old = identity(10, 100, 1000);
        let new = identity(10, 200, 2000);
        let mut registry = BorderRegistry::default();
        registry.insert(record(new, 12));

        assert!(!registry.set_state(old, BorderLifecycleState::Active));
        assert_eq!(
            registry.get_by_key(10).unwrap().state,
            BorderLifecycleState::Initializing
        );
        assert!(registry.set_state(new, BorderLifecycleState::Active));
        assert_eq!(
            registry.get_by_key(10).unwrap().state,
            BorderLifecycleState::Active
        );
    }
}

// Pure reconciliation planning is kept outside BorderRuntime so HWND reuse and create/destroy
// ordering can be unit-tested without a live desktop/window.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    pub destroy: Vec<WindowIdentity>,
    pub create: Vec<WindowIdentity>,
}

pub fn plan_reconciliation(
    current: &[BorderRecord],
    present: &HashMap<isize, WindowIdentity>,
    creatable: &HashSet<isize>,
) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();
    let current_by_hwnd: HashMap<isize, WindowIdentity> = current
        .iter()
        .map(|record| (record.tracking.hwnd, record.tracking))
        .collect();

    for record in current {
        match present.get(&record.tracking.hwnd) {
            Some(identity) if *identity == record.tracking => {}
            _ => plan.destroy.push(record.tracking),
        }
    }

    for (hwnd, identity) in present {
        let already_current = current_by_hwnd.get(hwnd) == Some(identity);
        if !already_current && creatable.contains(hwnd) {
            plan.create.push(*identity);
        }
    }

    // Determinism makes logs/tests easier to reason about; runtime always executes destroy first.
    plan.destroy
        .sort_by_key(|identity| (identity.hwnd, identity.process_id, identity.thread_id));
    plan.create
        .sort_by_key(|identity| (identity.hwnd, identity.process_id, identity.thread_id));
    plan
}

#[cfg(test)]
mod phase2_reconciliation_tests {
    use super::{
        BorderLifecycleState, BorderRecord, ReconcilePlan, WindowIdentity, plan_reconciliation,
    };
    use std::collections::{HashMap, HashSet};

    fn id(hwnd: isize, pid: u32, tid: u32) -> WindowIdentity {
        WindowIdentity {
            hwnd,
            process_id: pid,
            thread_id: tid,
        }
    }

    fn record(identity: WindowIdentity) -> BorderRecord {
        BorderRecord {
            tracking: identity,
            border_hwnd: identity.hwnd + 1000,
            state: BorderLifecycleState::Active,
        }
    }

    #[test]
    fn same_identity_is_kept_even_when_temporarily_not_creatable() {
        let identity = id(10, 1, 2);
        let present = HashMap::from([(identity.hwnd, identity)]);
        let plan = plan_reconciliation(&[record(identity)], &present, &HashSet::new());
        assert_eq!(plan, ReconcilePlan::default());
    }

    #[test]
    fn missing_identity_is_destroyed() {
        let old = id(10, 1, 2);
        let plan = plan_reconciliation(&[record(old)], &HashMap::new(), &HashSet::new());
        assert_eq!(plan.destroy, vec![old]);
        assert!(plan.create.is_empty());
    }

    #[test]
    fn missed_visible_window_is_created() {
        let new = id(10, 1, 2);
        let present = HashMap::from([(new.hwnd, new)]);
        let creatable = HashSet::from([new.hwnd]);
        let plan = plan_reconciliation(&[], &present, &creatable);
        assert_eq!(plan.create, vec![new]);
        assert!(plan.destroy.is_empty());
    }

    #[test]
    fn hwnd_reuse_destroys_old_identity_before_creating_new_identity() {
        let old = id(10, 1, 2);
        let new = id(10, 7, 8);
        let present = HashMap::from([(new.hwnd, new)]);
        let creatable = HashSet::from([new.hwnd]);
        let plan = plan_reconciliation(&[record(old)], &present, &creatable);
        assert_eq!(plan.destroy, vec![old]);
        assert_eq!(plan.create, vec![new]);
    }

    #[test]
    fn hwnd_reuse_to_hidden_window_only_destroys_old_border() {
        let old = id(10, 1, 2);
        let new = id(10, 7, 8);
        let present = HashMap::from([(new.hwnd, new)]);
        let plan = plan_reconciliation(&[record(old)], &present, &HashSet::new());
        assert_eq!(plan.destroy, vec![old]);
        assert!(plan.create.is_empty());
    }
}
