//! 실행 취소/다시 실행 기록. 매 프레임 현재 상태를 넘기면, 변경이 잠시 멈췄을 때 한 단계로 묶어 기록한다.
//! (글자를 칠 때마다 한 단계씩 쌓이지 않도록 디바운스한다.)

const MAX_STEPS: usize = 100;
/// 마지막 변경 후 이 시간(초)이 지나면 한 단계로 확정한다.
pub const SETTLE_SECS: f64 = 0.6;

pub struct History<T> {
    undo: Vec<T>,
    redo: Vec<T>,
    /// 마지막으로 확정된 상태
    committed: T,
    /// 아직 확정되지 않은 최신 상태와 그 변경 시각
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

    /// 매 프레임 호출한다. 확정을 기다리는 변경이 있으면 true(다시 그리기 예약용).
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

    /// 되돌릴 상태를 돌려준다. 확정 전 변경이 있으면 먼저 확정해서 그 변경부터 되돌린다.
    pub fn undo(&mut self) -> Option<T> {
        self.commit();
        let prev = self.undo.pop()?;
        let cur = std::mem::replace(&mut self.committed, prev);
        self.redo.push(cur);
        Some(self.committed.clone())
    }

    pub fn redo(&mut self) -> Option<T> {
        if self.pending.is_some() {
            return None; // 새 변경이 생기면 다시 실행 기록은 무효
        }
        let next = self.redo.pop()?;
        let cur = std::mem::replace(&mut self.committed, next);
        self.undo.push(cur);
        Some(self.committed.clone())
    }

    /// 프로젝트 불러오기처럼 기록 없이 상태를 바꿀 때 쓴다.
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
        // 0.1초 간격으로 한 글자씩: 아직 확정 안 됨
        for (i, s) in ["a", "ab", "abc"].iter().enumerate() {
            assert!(h.track(&s.to_string(), i as f64 * 0.1));
        }
        assert!(!h.track(&"abc".to_string(), 1.0)); // 멈춘 뒤 확정
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
        h.track(&3, 1.1); // 아직 확정 전
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
