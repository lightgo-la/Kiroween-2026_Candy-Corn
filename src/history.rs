//! Undo/redo history. Pass in the current state every frame; once changes pause briefly, they are recorded as one step.
//! (Debounced so that typing does not add one step per character.)

const MAX_STEPS: usize = 100;
/// A change is committed as one step once this many seconds have passed since the last change.
pub const SETTLE_SECS: f64 = 0.6;

pub struct History<T> {
    undo: Vec<T>,
    redo: Vec<T>,
    /// The last committed state
    committed: T,
    /// The latest uncommitted state and the time it changed
    pending: Option<(T, f64)>,
}

impl<T: Clone + PartialEq> History<T> {
    pub fn new(initial: T) -> Self {
        History {
            undo: vec![],
            redo: vec![],
            committed: initial,
            pending: None,
        }
    }

    /// Call every frame. Returns true while a change is waiting to be committed (used to schedule a repaint).
    pub fn track(&mut self, now: &T, time: f64) -> bool {
        if *now == self.committed {
            self.pending = None;
            return false;
        }
        match &mut self.pending {
            Some((p, t)) if p == now => {
                if time - *t >= SETTLE_SECS {
                    self.commit();
                    return false;
                }
            }
            _ => self.pending = Some((now.clone(), time)),
        }
        true
    }

    fn commit(&mut self) {
        if let Some((p, _)) = self.pending.take() {
            if p == self.committed {
                return;
            }
            let prev = std::mem::replace(&mut self.committed, p);
            self.undo.push(prev);
            if self.undo.len() > MAX_STEPS {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty() || self.pending.is_some()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty() && self.pending.is_none()
    }

    /// Returns the state to go back to. An uncommitted change is committed first, so it is the first one undone.
    pub fn undo(&mut self) -> Option<T> {
        self.commit();
        let prev = self.undo.pop()?;
        let cur = std::mem::replace(&mut self.committed, prev);
        self.redo.push(cur);
        Some(self.committed.clone())
    }

    pub fn redo(&mut self) -> Option<T> {
        if self.pending.is_some() {
            return None; // a new change invalidates the redo history
        }
        let next = self.redo.pop()?;
        let cur = std::mem::replace(&mut self.committed, next);
        self.undo.push(cur);
        Some(self.committed.clone())
    }

    /// Replaces the state without recording history, e.g. when a project is loaded.
    pub fn reset(&mut self, state: T) {
        *self = History::new(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_is_one_step_after_pause() {
        let mut h = History::new("".to_string());
        // One character every 0.1 s: not committed yet
        for (i, s) in ["a", "ab", "abc"].iter().enumerate() {
            assert!(h.track(&s.to_string(), i as f64 * 0.1));
        }
        assert!(!h.track(&"abc".to_string(), 1.0)); // committed after the pause
        assert_eq!(h.undo().as_deref(), Some(""));
        assert_eq!(h.redo().as_deref(), Some("abc"));
        assert!(h.undo().is_some());
        assert!(h.undo().is_none());
    }

    #[test]
    fn undo_commits_pending_change_first() {
        let mut h = History::new(1);
        h.track(&2, 0.0);
        h.track(&2, 1.0);
        h.track(&3, 1.1); // not committed yet
        assert_eq!(h.undo(), Some(2));
        assert_eq!(h.undo(), Some(1));
        assert_eq!(h.redo(), Some(2));
    }

    #[test]
    fn new_change_clears_redo_and_caps_length() {
        let mut h = History::new(0);
        for i in 1..=150 {
            h.track(&i, i as f64);
            h.track(&i, i as f64 + 1.0);
        }
        let mut n = 0;
        while h.undo().is_some() {
            n += 1;
        }
        assert_eq!(n, MAX_STEPS);
        h.track(&999, 1000.0);
        h.track(&999, 1001.0);
        assert!(!h.can_redo());
    }

    #[test]
    fn returning_to_committed_state_drops_pending() {
        let mut h = History::new(5);
        assert!(h.track(&6, 0.0));
        assert!(!h.track(&5, 0.1));
        assert!(!h.can_undo());
    }
}
