//! Live readings: quota numbers that a CLI already holds (Claude Code's status line input, Codex's session
//! logs), applied to `State` without a request to the usage endpoint.
//!
//! Every source goes through `record`, so the rules live in one place:
//! * A reading no newer than the stored one, or for a `Dead` account, is ignored.
//! * A taken reading marks the account `Ok` and pushes `next_poll_at` out to `MIN_INTERVAL_S` after the
//!   observation, so the poller spends no request on an account with fresh live data. When the live source
//!   goes quiet the poller resumes at its own cadence.
//! * A source that repeats itself (the status line runs after every response) does not rewrite `state.json`:
//!   an unchanged reading under `REWRITE_AFTER_S` old is skipped.

use crate::error::Result;
use crate::paths::Paths;
use crate::policy::MIN_INTERVAL_S;
use crate::state::{State, UsageState, Windows};

/// An unchanged reading is written again only when the stored one is this old, so `list` shows a fresh age.
pub const REWRITE_AFTER_S: u64 = 60;

/// Whether `windows` observed at `observed_at` would change the record of `account_id`.
fn takes(state: &State, account_id: &str, windows: &Windows, observed_at: u64) -> bool {
    if windows.five_hour.is_none() && windows.weekly.is_none() {
        return false;
    }
    let Some(prev) = state.usage.get(account_id) else { return true };
    if prev.state == UsageState::Dead {
        return false;
    }
    let Some(fetched_at) = prev.fetched_at else { return true };
    if observed_at <= fetched_at {
        return false;
    }
    let repeat = prev.state == UsageState::Ok && prev.detail.is_none() && prev.windows == *windows;
    !(repeat && observed_at - fetched_at < REWRITE_AFTER_S)
}

/// Apply a live reading to `state`. Returns whether the record changed.
pub fn apply(state: &mut State, account_id: &str, windows: Windows, observed_at: u64) -> bool {
    if !takes(state, account_id, &windows, observed_at) {
        return false;
    }
    let rec = state.usage.entry(account_id.to_string()).or_default();
    rec.state = UsageState::Ok;
    rec.windows = windows;
    rec.fetched_at = Some(observed_at);
    rec.checked_at = rec.checked_at.max(observed_at);
    rec.detail = None;
    rec.next_poll_at = rec.next_poll_at.max(observed_at + MIN_INTERVAL_S);
    true
}

/// Apply a live reading to the state file under its lock. Reads first, so a repeated reading costs no write.
pub fn record(paths: &Paths, account_id: &str, windows: Windows, observed_at: u64) -> Result<bool> {
    if !takes(&State::load(paths)?, account_id, &windows, observed_at) {
        return Ok(false);
    }
    State::update(paths, |s| apply(s, account_id, windows, observed_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{UsageRecord, WindowSnap};

    const T: u64 = 1_800_000_000;

    fn win(five: f64) -> Windows {
        Windows { five_hour: Some(WindowSnap { pct: five, resets_at: Some(T as i64 + 3600) }), weekly: None }
    }

    fn state_with(rec: UsageRecord) -> State {
        let mut s = State::default();
        s.usage.insert("a".into(), rec);
        s
    }

    fn stored(fetched_at: u64, pct: f64) -> UsageRecord {
        UsageRecord { state: UsageState::Ok, windows: win(pct), fetched_at: Some(fetched_at), checked_at: fetched_at, next_poll_at: fetched_at + 100, interval_s: 240, ..UsageRecord::default() }
    }

    #[test]
    fn a_reading_for_an_unknown_account_creates_an_ok_record() {
        let mut s = State::default();
        assert!(apply(&mut s, "a", win(12.0), T));
        let r = &s.usage["a"];
        assert_eq!((r.state, r.windows, r.fetched_at), (UsageState::Ok, win(12.0), Some(T)));
        assert_eq!(r.next_poll_at, T + MIN_INTERVAL_S);
    }

    #[test]
    fn a_reading_that_is_not_newer_is_ignored() {
        let mut s = state_with(stored(T, 10.0));
        assert!(!apply(&mut s, "a", win(50.0), T));
        assert!(!apply(&mut s, "a", win(50.0), T - 5));
        assert_eq!(s.usage["a"].windows, win(10.0));
    }

    #[test]
    fn a_dead_account_ignores_live_readings() {
        let mut s = state_with(UsageRecord { state: UsageState::Dead, detail: Some("dead".into()), ..stored(T - 500, 10.0) });
        assert!(!apply(&mut s, "a", win(50.0), T));
        assert_eq!(s.usage["a"].state, UsageState::Dead);
    }

    #[test]
    fn a_reading_with_no_window_is_ignored() {
        let mut s = State::default();
        assert!(!apply(&mut s, "a", Windows::default(), T));
        assert!(s.usage.is_empty());
    }

    #[test]
    fn a_live_reading_replaces_a_failed_one_and_keeps_the_last_429() {
        let failed = UsageRecord { state: UsageState::Unreadable, detail: Some("http-429".into()), last_429_at: Some(T - 30), next_poll_at: T + 900, ..stored(T - 400, 10.0) };
        let mut s = state_with(failed);
        assert!(apply(&mut s, "a", win(20.0), T));
        let r = &s.usage["a"];
        assert_eq!((r.state, r.detail.as_deref(), r.fetched_at), (UsageState::Ok, None, Some(T)));
        assert_eq!(r.last_429_at, Some(T - 30));
        assert_eq!(r.next_poll_at, T + 900, "a later poll time is not pulled in");
    }

    #[test]
    fn the_next_poll_moves_out_to_the_floor_after_the_observation() {
        let mut s = state_with(UsageRecord { next_poll_at: T + 10, ..stored(T - 100, 10.0) });
        assert!(apply(&mut s, "a", win(11.0), T));
        assert_eq!(s.usage["a"].next_poll_at, T + MIN_INTERVAL_S);
        assert_eq!(s.usage["a"].interval_s, 240, "the poller's own interval is left alone");
    }

    #[test]
    fn an_unchanged_reading_is_written_only_once_the_stored_one_is_a_minute_old() {
        let mut s = state_with(stored(T, 10.0));
        assert!(!apply(&mut s, "a", win(10.0), T + REWRITE_AFTER_S - 1));
        assert!(apply(&mut s, "a", win(10.0), T + REWRITE_AFTER_S));
        assert_eq!(s.usage["a"].fetched_at, Some(T + REWRITE_AFTER_S));
        assert!(apply(&mut s, "a", win(10.5), T + REWRITE_AFTER_S + 1), "a changed value is written at once");
    }

    #[test]
    fn record_does_not_touch_the_state_file_for_a_repeat() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        assert!(record(&paths, "a", win(10.0), T).unwrap());
        let before = std::fs::metadata(paths.state_file()).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!record(&paths, "a", win(10.0), T + 5).unwrap());
        assert_eq!(std::fs::metadata(paths.state_file()).unwrap().modified().unwrap(), before);
        assert_eq!(State::load(&paths).unwrap().usage["a"].fetched_at, Some(T));
    }
}
