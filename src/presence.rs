// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The "Are you still there?" timer: proof that an operator is at the station while an
//! Auto mode is on. Pure logic; the app shows the dialogs.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ok,
    /// Time to ask: within `warn` of the deadline.
    Ask,
    Expired,
}

pub struct PresenceTimer {
    /// seconds from arming to the deadline
    pub interval: f64,
    /// the question is asked this long before it
    pub warn: f64,
    clock: Box<dyn Fn() -> f64>,
    deadline: Option<f64>,
}

impl PresenceTimer {
    pub fn new(interval: f64, warn: f64, clock: Box<dyn Fn() -> f64>) -> Self {
        Self { interval, warn, clock, deadline: None }
    }

    /// Start (or restart) the countdown from now.
    pub fn arm(&mut self) {
        self.deadline = Some((self.clock)() + self.interval);
    }

    pub fn disarm(&mut self) {
        self.deadline = None;
    }

    #[cfg(test)]
    pub fn armed(&self) -> bool {
        self.deadline.is_some()
    }

    /// Seconds to the deadline (0 once passed), None when not armed.
    pub fn remaining(&self) -> Option<f64> {
        self.deadline.map(|d| (d - (self.clock)()).max(0.0))
    }

    /// None (not armed), Ok, Ask or Expired.
    pub fn phase(&self) -> Option<Phase> {
        let left = self.remaining()?;
        Some(if left <= 0.0 {
            Phase::Expired
        } else if left <= self.warn {
            Phase::Ask
        } else {
            Phase::Ok
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn timer() -> (Rc<Cell<f64>>, PresenceTimer) {
        let now = Rc::new(Cell::new(1000.0));
        let n = now.clone();
        (now, PresenceTimer::new(120.0, 60.0, Box::new(move || n.get())))
    }

    #[test]
    fn not_armed() {
        let (_, t) = timer();
        assert_eq!(t.phase(), None);
        assert_eq!(t.remaining(), None);
    }

    #[test]
    fn phases() {
        let (now, mut t) = timer();
        t.arm();
        assert_eq!(t.phase(), Some(Phase::Ok));
        now.set(now.get() + 59.9);
        assert_eq!(t.phase(), Some(Phase::Ok));
        now.set(now.get() + 0.2);
        assert_eq!(t.phase(), Some(Phase::Ask));
        now.set(now.get() + 59.7);
        assert_eq!(t.phase(), Some(Phase::Ask));
        now.set(now.get() + 0.5);
        assert_eq!(t.phase(), Some(Phase::Expired));
        assert_eq!(t.remaining(), Some(0.0));
    }

    #[test]
    fn rearm_restarts() {
        let (now, mut t) = timer();
        t.arm();
        now.set(now.get() + 100.0);
        assert_eq!(t.phase(), Some(Phase::Ask));
        t.arm();
        assert_eq!(t.phase(), Some(Phase::Ok));
    }

    #[test]
    fn disarm() {
        let (_, mut t) = timer();
        t.arm();
        t.disarm();
        assert!(!t.armed());
        assert_eq!(t.phase(), None);
    }
}
