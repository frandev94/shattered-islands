//! Pure re-assert guard: decides whether a managed window is left alone, captured,
//! re-asserted (with backoff) or abandoned because the game fights back.
//!
//! No Win32 and no real clock: every method receives `now_ms` from the caller, so the
//! timeline of any measured game (`docs/GAMES.md`) can be replayed deterministically.

use std::collections::VecDeque;

use crate::window_state::Rect;

/// Rolling window for the watch-phase cycle budget.
const MINUTE_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardConfig {
    /// Quiet time after the last write before the window counts as settled.
    pub settle_ms: u64,
    /// Re-asserts allowed in one cycle.
    pub max_reasserts: u32,
    /// Wait after detecting a deviation, per re-assert (index = re-asserts already done).
    pub backoff_ms: [u64; 3],
    /// Watch-phase cycles allowed per rolling minute.
    pub cycles_per_minute: u32,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            settle_ms: 600,
            max_reasserts: 3,
            backoff_ms: [50, 150, 400],
            cycles_per_minute: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Excluded,
    NotTopLevelVisible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip(SkipReason),
    AlreadyPlaced,
    /// Capture the original state and place (unmanaged), or enter the re-assert loop (managed).
    Capture,
}

/// Entry decision: excluded or hidden windows are left alone; a managed window already at its
/// target without a frame needs no write; anything else is captured, even a window that already
/// has no frame (a game started with `-popupwindow` still needs placing).
pub fn decide(
    excluded: bool,
    top_level_visible: bool,
    managed: bool,
    at_target: bool,
    has_frame: bool,
) -> Decision {
    if excluded {
        Decision::Skip(SkipReason::Excluded)
    } else if !top_level_visible {
        Decision::Skip(SkipReason::NotTopLevelVisible)
    } else if managed && at_target && !has_frame {
        Decision::AlreadyPlaced
    } else {
        Decision::Capture
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    AtTarget,
    Deviated { rect: Rect, frame_back: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Re-assert at this time (ms); call `on_reasserted` after doing it.
    ReassertAt(u64),
    /// Stop fighting; `forced` is the rect the game imposes.
    GiveUp {
        forced: Rect,
    },
    /// Settle window elapsed without deviation.
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Cycle in progress (initial apply or a watch cycle): waiting for settle.
    Active,
    /// Settled; a deviation starts a new, budgeted cycle.
    Watching,
    /// Gave up; nothing happens until `reset`.
    Abandoned,
}

#[derive(Debug, Clone)]
pub struct Guard {
    cfg: GuardConfig,
    phase: Phase,
    settle_until: u64,
    reasserts: u32,
    pending: Option<u64>,
    last_forced: Option<Rect>,
    /// Start times of watch cycles inside the last minute.
    cycles: VecDeque<u64>,
    fights_back: bool,
}

impl Guard {
    /// Starts the initial cycle: the caller has just written the target at `now_ms`. The target
    /// itself is the engine's concern; the guard only sees observations.
    pub fn new(cfg: GuardConfig, _target: Rect, now_ms: u64) -> Self {
        Self {
            cfg,
            phase: Phase::Active,
            settle_until: now_ms + cfg.settle_ms,
            reasserts: 0,
            pending: None,
            last_forced: None,
            cycles: VecDeque::new(),
            fights_back: false,
        }
    }

    pub fn fights_back(&self) -> bool {
        self.fights_back
    }

    /// Explicit apply/place: new target, all counters and `fights_back` cleared.
    pub fn reset(&mut self, _target: Rect, now_ms: u64) {
        *self = Self::new(self.cfg, _target, now_ms);
    }

    pub fn on_observation(&mut self, now_ms: u64, obs: Observation) -> Action {
        if self.phase == Phase::Abandoned {
            return Action::None;
        }
        match obs {
            Observation::AtTarget => {
                // The game conformed on its own: drop the planned re-assert.
                self.pending = None;
                self.check_settled(now_ms)
            }
            Observation::Deviated { rect, .. } => self.on_deviation(now_ms, rect),
        }
    }

    /// The caller performed the re-assert requested by `ReassertAt`.
    /// Ignored when no re-assert was pending (cancelled by an `AtTarget` observation).
    pub fn on_reasserted(&mut self, now_ms: u64) {
        if self.phase == Phase::Abandoned || self.pending.take().is_none() {
            return;
        }
        self.reasserts += 1;
        self.settle_until = now_ms + self.cfg.settle_ms;
    }

    pub fn on_tick(&mut self, now_ms: u64) -> Action {
        if self.phase == Phase::Abandoned {
            return Action::None;
        }
        match self.pending {
            Some(at) if now_ms >= at => Action::ReassertAt(at),
            Some(_) => Action::None,
            None => self.check_settled(now_ms),
        }
    }

    fn check_settled(&mut self, now_ms: u64) -> Action {
        if self.phase == Phase::Active && self.pending.is_none() && now_ms >= self.settle_until {
            self.phase = Phase::Watching;
            Action::Settled
        } else {
            Action::None
        }
    }

    fn on_deviation(&mut self, now_ms: u64, rect: Rect) -> Action {
        // Sequence of reverts already being waited out: keep the planned time.
        if let Some(at) = self.pending {
            return Action::ReassertAt(at);
        }
        // A deviation after the settle window belongs to a new watch cycle.
        if self.phase == Phase::Active && now_ms >= self.settle_until {
            self.phase = Phase::Watching;
        }
        if self.phase == Phase::Watching {
            while self
                .cycles
                .front()
                .is_some_and(|&t| now_ms.saturating_sub(t) >= MINUTE_MS)
            {
                self.cycles.pop_front();
            }
            if self.cycles.len() as u32 >= self.cfg.cycles_per_minute {
                return self.give_up(rect);
            }
            self.cycles.push_back(now_ms);
            self.phase = Phase::Active;
            self.reasserts = 0;
            self.last_forced = None;
        }

        // Same forced rect in response to two re-asserts in a row: the game defends itself.
        let repeated = self.reasserts >= 2 && self.last_forced == Some(rect);
        self.last_forced = Some(rect);
        if repeated || self.reasserts >= self.cfg.max_reasserts {
            return self.give_up(rect);
        }
        let wait =
            self.cfg.backoff_ms[(self.reasserts as usize).min(self.cfg.backoff_ms.len() - 1)];
        let at = now_ms + wait;
        self.pending = Some(at);
        Action::ReassertAt(at)
    }

    fn give_up(&mut self, forced: Rect) -> Action {
        self.phase = Phase::Abandoned;
        self.pending = None;
        self.fights_back = true;
        Action::GiveUp { forced }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: Rect = Rect {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };
    const FORCED: Rect = Rect {
        left: 100,
        top: 100,
        right: 1380,
        bottom: 820,
    };

    fn guard() -> Guard {
        Guard::new(GuardConfig::default(), TARGET, 0)
    }

    fn dev(rect: Rect, frame_back: bool) -> Observation {
        Observation::Deviated { rect, frame_back }
    }

    #[test]
    fn decide_truth_table() {
        for bits in 0..32u8 {
            let b = |i: u8| bits & (1 << i) != 0;
            let (excluded, visible, managed, at_target, has_frame) = (b(0), b(1), b(2), b(3), b(4));
            let expected = if excluded {
                Decision::Skip(SkipReason::Excluded)
            } else if !visible {
                Decision::Skip(SkipReason::NotTopLevelVisible)
            } else if managed && at_target && !has_frame {
                Decision::AlreadyPlaced
            } else {
                Decision::Capture
            };
            assert_eq!(
                decide(excluded, visible, managed, at_target, has_frame),
                expected,
                "{bits:05b}"
            );
        }
        // Spot checks of the documented rules.
        assert_eq!(
            decide(true, false, true, true, false),
            Decision::Skip(SkipReason::Excluded)
        );
        assert_eq!(
            decide(false, false, false, false, true),
            Decision::Skip(SkipReason::NotTopLevelVisible)
        );
        assert_eq!(
            decide(false, true, true, true, false),
            Decision::AlreadyPlaced
        );
        // Unmanaged windows are always captured, even if already borderless at target.
        assert_eq!(decide(false, true, false, true, false), Decision::Capture);
        // Managed but deviated (rect or frame) goes to the re-assert loop.
        assert_eq!(decide(false, true, true, false, false), Decision::Capture);
        assert_eq!(decide(false, true, true, true, true), Decision::Capture);
    }

    #[test]
    fn notepad_settles_after_600ms() {
        let mut g = guard();
        assert_eq!(g.on_observation(10, Observation::AtTarget), Action::None);
        assert_eq!(g.on_tick(599), Action::None);
        assert_eq!(g.on_tick(600), Action::Settled);
        assert_eq!(g.on_tick(700), Action::None);
        assert!(!g.fights_back());
    }

    #[test]
    fn warscape_same_forced_rect_gives_up_after_second_reassert() {
        let mut g = guard();
        // Style + size reverted at ~20 ms.
        assert_eq!(
            g.on_observation(20, dev(FORCED, true)),
            Action::ReassertAt(70)
        );
        assert_eq!(g.on_tick(69), Action::None);
        assert_eq!(g.on_tick(70), Action::ReassertAt(70));
        g.on_reasserted(70);
        // Reverted again 7 ms after the re-assert.
        assert_eq!(
            g.on_observation(77, dev(FORCED, true)),
            Action::ReassertAt(227)
        );
        g.on_reasserted(227);
        assert!(!g.fights_back());
        assert_eq!(
            g.on_observation(234, dev(FORCED, true)),
            Action::GiveUp { forced: FORCED }
        );
        assert!(g.fights_back());
        // Abandoned: further events are ignored.
        assert_eq!(g.on_observation(300, dev(FORCED, true)), Action::None);
        assert_eq!(g.on_tick(5000), Action::None);
    }

    #[test]
    fn unity_size_forced_twice_stops_early() {
        let unity = Rect {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let mut g = guard();
        assert_eq!(
            g.on_observation(60, dev(unity, false)),
            Action::ReassertAt(110)
        );
        g.on_reasserted(110);
        assert_eq!(
            g.on_observation(186, dev(unity, false)),
            Action::ReassertAt(336)
        );
        g.on_reasserted(336);
        // 3rd re-assert (400 ms) is never needed.
        assert_eq!(
            g.on_observation(340, dev(unity, false)),
            Action::GiveUp { forced: unity }
        );
        assert!(g.fights_back());
    }

    #[test]
    fn backoff_is_50_150_400_then_gives_up_on_max() {
        let (a, b, c, d) = (
            Rect { left: 1, ..FORCED },
            Rect { left: 2, ..FORCED },
            Rect { left: 3, ..FORCED },
            Rect { left: 4, ..FORCED },
        );
        let mut g = guard();
        assert_eq!(
            g.on_observation(100, dev(a, false)),
            Action::ReassertAt(150)
        );
        g.on_reasserted(150);
        assert_eq!(
            g.on_observation(200, dev(b, false)),
            Action::ReassertAt(350)
        );
        g.on_reasserted(350);
        assert_eq!(
            g.on_observation(400, dev(c, false)),
            Action::ReassertAt(800)
        );
        g.on_reasserted(800);
        assert_eq!(
            g.on_observation(900, dev(d, false)),
            Action::GiveUp { forced: d }
        );
        assert!(g.fights_back());
    }

    #[test]
    fn deviation_while_waiting_keeps_planned_time() {
        let mut g = guard();
        assert_eq!(
            g.on_observation(60, dev(FORCED, false)),
            Action::ReassertAt(110)
        );
        assert_eq!(
            g.on_observation(80, dev(TARGET, true)),
            Action::ReassertAt(110)
        );
        g.on_reasserted(110);
        // Only one re-assert counted.
        assert_eq!(
            g.on_observation(120, dev(FORCED, false)),
            Action::ReassertAt(270)
        );
    }

    #[test]
    fn game_conforming_cancels_pending_reassert() {
        let mut g = guard();
        assert_eq!(
            g.on_observation(60, dev(FORCED, false)),
            Action::ReassertAt(110)
        );
        assert_eq!(g.on_observation(80, Observation::AtTarget), Action::None);
        assert_eq!(g.on_tick(110), Action::None);
        g.on_reasserted(110); // no-op: nothing pending
        assert_eq!(g.on_tick(600), Action::Settled);
    }

    #[test]
    fn reassert_restarts_settle_window() {
        let mut g = guard();
        assert_eq!(
            g.on_observation(500, dev(FORCED, false)),
            Action::ReassertAt(550)
        );
        assert_eq!(g.on_tick(600), Action::ReassertAt(550)); // still pending: not settled
        g.on_reasserted(550);
        assert_eq!(g.on_tick(1149), Action::None);
        assert_eq!(g.on_tick(1150), Action::Settled);
    }

    #[test]
    fn late_frame_is_one_watch_cycle() {
        let mut g = guard();
        assert_eq!(g.on_tick(600), Action::Settled);
        assert_eq!(
            g.on_observation(1500, dev(TARGET, true)),
            Action::ReassertAt(1550)
        );
        g.on_reasserted(1550);
        assert_eq!(g.on_observation(1560, Observation::AtTarget), Action::None);
        assert_eq!(g.on_tick(2150), Action::Settled);
        assert!(!g.fights_back());
    }

    #[test]
    fn fourth_cycle_in_a_minute_marks_fights_back() {
        let mut g = guard();
        assert_eq!(g.on_tick(600), Action::Settled);
        for (i, t) in [1500u64, 3000, 4500].into_iter().enumerate() {
            let rect = Rect {
                left: i as i32,
                ..FORCED
            }; // distinct, so never "same rect twice"
            assert_eq!(
                g.on_observation(t, dev(rect, true)),
                Action::ReassertAt(t + 50),
                "cycle {i}"
            );
            g.on_reasserted(t + 50);
            assert_eq!(g.on_tick(t + 50 + 600), Action::Settled);
        }
        assert!(!g.fights_back());
        assert_eq!(
            g.on_observation(6000, dev(FORCED, true)),
            Action::GiveUp { forced: FORCED }
        );
        assert!(g.fights_back());
    }

    #[test]
    fn cycles_older_than_a_minute_do_not_count() {
        let mut g = guard();
        assert_eq!(g.on_tick(600), Action::Settled);
        for t in [1000u64, 2000, 3000] {
            assert!(matches!(
                g.on_observation(t, dev(TARGET, true)),
                Action::ReassertAt(_)
            ));
            g.on_reasserted(t + 50);
            assert_eq!(g.on_tick(t + 700), Action::Settled);
        }
        // 1000 and 2000 expired by 62_500; only 3000 counts.
        assert_eq!(
            g.on_observation(62_500, dev(TARGET, true)),
            Action::ReassertAt(62_550)
        );
        assert!(!g.fights_back());
    }

    #[test]
    fn reset_clears_counters_and_fights_back() {
        let mut g = guard();
        g.on_observation(20, dev(FORCED, true));
        g.on_reasserted(70);
        g.on_observation(77, dev(FORCED, true));
        g.on_reasserted(227);
        assert_eq!(
            g.on_observation(234, dev(FORCED, true)),
            Action::GiveUp { forced: FORCED }
        );
        assert!(g.fights_back());

        let other = Rect {
            left: 5,
            top: 5,
            right: 105,
            bottom: 105,
        };
        g.reset(other, 1000);
        assert!(!g.fights_back());

        // Fresh cycle: first backoff again, full settle window from the reset.
        assert_eq!(
            g.on_observation(1020, dev(FORCED, true)),
            Action::ReassertAt(1070)
        );
        g.on_reasserted(1070);
        assert_eq!(g.on_tick(1670), Action::Settled);
    }
}
