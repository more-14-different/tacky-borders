use std::collections::HashMap;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
