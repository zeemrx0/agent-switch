//! Codex session logs as a source of live quota readings.
//!
//! Codex appends one JSON line per event to `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (the shallow
//! Codex home links `sessions` to it, so one folder covers the app and the CLI). Two kinds of line matter:
//! * the first, `session_meta`, whose `payload.creator_account_id` is the ChatGPT account id of the login that
//!   started the session;
//! * `event_msg` lines with `payload.type == "token_count"`, whose `payload.rate_limits` is
//!   `{limit_id: "codex", primary: {used_percent, window_minutes, resets_at}, secondary: {...}}` with
//!   `resets_at` in Unix seconds. The numbers are the account's quota as the server last reported it.
//!
//! A pass reads the last such line of each file that changed in the last `RECENT_S` seconds, credits it to
//! the saved OpenAI account with that ChatGPT account id, and gives it to `live_usage::record` stamped with
//! the line's own time. Only the head and the tail of a file are read, never the whole of a large one.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::config::{Account, Config, Provider, SurfaceKind};
use crate::error::Result;
use crate::live_usage;
use crate::paths::Paths;
use crate::providers::openai::parse_rfc3339_us;
use crate::state::{State, WindowSnap, Windows};

/// Only files changed this recently are read: an older session holds nothing newer than the poller's reading.
pub const RECENT_S: u64 = 600;
/// How much of the end of a file is searched for the last `token_count` line.
const TAIL_BYTES: u64 = 512 * 1024;
/// The first line carries the session's instructions and can be long; a line past this is not read.
const HEAD_LINE_BYTES: u64 = 1 << 20;
/// A window of this many minutes or fewer is the 5-hour window, a longer one the weekly window.
const FIVE_HOUR_MAX_MINUTES: u64 = 6 * 60;

/// One quota reading found in a session file.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// `creator_account_id` of the session.
    pub chatgpt_account_id: String,
    pub windows: Windows,
    /// Unix seconds of the event.
    pub observed_at: u64,
}

fn window(w: &Value) -> Option<(WindowSnap, Option<u64>)> {
    let pct = w.get("used_percent")?.as_f64().filter(|p| p.is_finite() && *p >= 0.0)?;
    Some((WindowSnap { pct, resets_at: w.get("resets_at").and_then(Value::as_i64) }, w.get("window_minutes").and_then(Value::as_u64)))
}

/// The windows and time of one `token_count` line, or `None` for any other line and for rate limits of
/// another limit id.
fn parse_token_count(line: &str) -> Option<(Windows, u64)> {
    if !line.contains("token_count") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload").filter(|p| p.get("type").and_then(Value::as_str) == Some("token_count"))?;
    let limits = payload.get("rate_limits").filter(|l| l.get("limit_id").and_then(Value::as_str) == Some("codex"))?;
    let observed_at = u64::try_from(parse_rfc3339_us(v.get("timestamp")?.as_str()?)?.div_euclid(1_000_000)).ok()?;
    let mut windows = Windows::default();
    // The window length says which window is which; without it primary is the 5-hour one.
    for (key, default_is_five_hour) in [("primary", true), ("secondary", false)] {
        let Some((snap, minutes)) = limits.get(key).filter(|w| !w.is_null()).and_then(window) else { continue };
        let slot = if minutes.map_or(default_is_five_hour, |m| m <= FIVE_HOUR_MAX_MINUTES) { &mut windows.five_hour } else { &mut windows.weekly };
        slot.get_or_insert(snap);
    }
    (windows.five_hour.is_some() || windows.weekly.is_some()).then_some((windows, observed_at))
}

/// The ChatGPT account id in a `session_meta` line.
fn parse_creator(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let id = v.get("payload")?.get("creator_account_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// The newest reading in one session file, or `None` when it has no creator or no `token_count` line.
fn read_session(path: &Path, len: u64) -> Option<Reading> {
    let file = File::open(path).ok()?;
    let mut head = String::new();
    BufReader::new(&file).take(HEAD_LINE_BYTES).read_line(&mut head).ok()?;
    let chatgpt_account_id = parse_creator(&head)?;

    let start = len.saturating_sub(TAIL_BYTES);
    let mut tail = Vec::new();
    let mut file = file;
    file.seek(SeekFrom::Start(start)).ok()?;
    file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let mut lines = tail.lines();
    if start > 0 {
        // The tail begins inside a line.
        lines.next();
    }
    let (windows, observed_at) = lines.rev().find_map(parse_token_count)?;
    Some(Reading { chatgpt_account_id, windows, observed_at })
}

/// Every `rollout-*.jsonl` under `dir`, at any depth. Links are not followed.
fn rollout_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && name.starts_with("rollout-") && name.ends_with(".jsonl") {
                found.push(entry.path());
            }
        }
    }
    found
}

/// The saved OpenAI account that owns `chatgpt_account_id`. Two saved logins can share a workspace id; the
/// one active on a Codex surface wins, and when that does not decide, nobody is credited.
fn account_for<'a>(config: &'a Config, state: &State, chatgpt_account_id: &str) -> Option<&'a Account> {
    let mut owners = config.accounts.iter().filter(|a| a.provider == Provider::Openai && a.uuid.as_deref() == Some(chatgpt_account_id));
    let first = owners.next()?;
    let Some(second) = owners.next() else { return Some(first) };
    let active = [SurfaceKind::CodexCli, SurfaceKind::ChatgptDesktop].into_iter().filter_map(|k| state.active(k));
    let live: Vec<&Account> = [first, second].into_iter().chain(owners).filter(|a| active.clone().any(|id| id == a.id)).collect();
    (live.len() == 1).then(|| live[0])
}

/// Reads the session folder on every pass. Remembers each file's size and time, so a file that did not change
/// is not read again.
#[derive(Default)]
pub struct CodexSessions {
    seen: HashMap<PathBuf, (u64, SystemTime)>,
}

impl CodexSessions {
    pub fn new() -> CodexSessions {
        CodexSessions::default()
    }

    /// Read the recent session files and apply what they hold. Returns the number of accounts updated.
    pub fn ingest(&mut self, paths: &Paths, config: &Config, now: u64) -> Result<usize> {
        if !config.accounts.iter().any(|a| a.provider == Provider::Openai) {
            return Ok(0);
        }
        let state = State::load(paths)?;
        let mut still_there = HashMap::new();
        // The newest reading of each account across all files.
        let mut newest: HashMap<String, Reading> = HashMap::new();
        for path in rollout_files(&paths.codex_sessions_dir()) {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let Ok(modified) = meta.modified() else { continue };
            let age = SystemTime::now().duration_since(modified).map(|d| d.as_secs()).unwrap_or(0);
            if age > RECENT_S {
                continue;
            }
            let stamp = (meta.len(), modified);
            still_there.insert(path.clone(), stamp);
            if self.seen.get(&path) == Some(&stamp) {
                continue;
            }
            let Some(reading) = read_session(&path, meta.len()) else { continue };
            let Some(account) = account_for(config, &state, &reading.chatgpt_account_id) else { continue };
            if newest.get(&account.id).is_none_or(|r| r.observed_at < reading.observed_at) {
                newest.insert(account.id.clone(), reading);
            }
        }
        self.seen = still_there;
        let mut updated = 0;
        for (id, reading) in newest {
            // An event time in the future would block every later reading.
            if live_usage::record(paths, &id, reading.windows, reading.observed_at.min(now))? {
                updated += 1;
            }
        }
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_800_000_000;

    fn meta_line(account: &str) -> String {
        format!(r#"{{"timestamp":"2027-01-15T08:00:00.000Z","ordinal":0,"type":"session_meta","payload":{{"creator_user_id":"user-1","creator_account_id":"{account}","id":"s1"}}}}"#)
    }

    /// A `token_count` line at `ts` (an RFC 3339 time) in the shape Codex writes.
    fn token_line(ts: &str, limit_id: &str, five: f64, week: f64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","ordinal":40,"type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"{limit_id}","limit_name":null,"primary":{{"used_percent":{five},"window_minutes":300,"resets_at":1800003600}},"secondary":{{"used_percent":{week},"window_minutes":10080,"resets_at":1800400000}},"plan_type":"team"}}}}}}"#
        )
    }

    // 2027-01-15T08:00:00Z
    const TS0: &str = "2027-01-15T08:00:00.500Z";
    const TS0_SECS: u64 = 1_800_000_000;

    #[test]
    fn a_token_count_line_gives_both_windows_by_length_and_the_event_time() {
        let (w, at) = parse_token_count(&token_line(TS0, "codex", 28.0, 4.0)).unwrap();
        assert_eq!(at, TS0_SECS);
        assert_eq!(w.five_hour, Some(WindowSnap { pct: 28.0, resets_at: Some(1_800_003_600) }));
        assert_eq!(w.weekly, Some(WindowSnap { pct: 4.0, resets_at: Some(1_800_400_000) }));
    }

    #[test]
    fn a_weekly_only_plan_puts_the_primary_window_in_the_weekly_slot_by_its_length() {
        let line = r#"{"timestamp":"2027-01-15T08:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":9.0,"window_minutes":10080,"resets_at":1800400000},"secondary":null}}}"#;
        let (w, _) = parse_token_count(line).unwrap();
        assert_eq!((w.five_hour, w.weekly.map(|s| s.pct)), (None, Some(9.0)));
    }

    #[test]
    fn other_lines_and_other_limit_ids_are_not_readings() {
        assert!(parse_token_count(&meta_line("acct-1")).is_none());
        assert!(parse_token_count(&token_line(TS0, "codex_other", 1.0, 1.0)).is_none());
        assert!(parse_token_count(r#"{"timestamp":"2027-01-15T08:00:00Z","payload":{"type":"token_count","info":null}}"#).is_none());
        assert!(parse_token_count(r#"{"payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":1.0}}}}"#).is_none(), "no timestamp");
        assert!(parse_token_count("token_count but not json").is_none());
        assert!(parse_token_count(&token_line("yesterday", "codex", 1.0, 1.0)).is_none());
    }

    #[test]
    fn the_creator_is_the_chatgpt_account_id_of_the_first_line() {
        assert_eq!(parse_creator(&meta_line("acct-1")).as_deref(), Some("acct-1"));
        assert_eq!(parse_creator(&meta_line("")), None);
        assert_eq!(parse_creator(r#"{"payload":{}}"#), None);
        assert_eq!(parse_creator("{"), None);
    }

    fn write_session(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn a_session_file_gives_its_last_reading() {
        let d = tempfile::tempdir().unwrap();
        let f = write_session(
            d.path(),
            "rollout-a.jsonl",
            &[meta_line("acct-1"), token_line("2027-01-15T07:59:00Z", "codex", 10.0, 2.0), token_line(TS0, "codex", 12.0, 2.0), r#"{"type":"event_msg","payload":{"type":"agent_message"}}"#.into()],
        );
        let r = read_session(&f, std::fs::metadata(&f).unwrap().len()).unwrap();
        assert_eq!((r.chatgpt_account_id.as_str(), r.observed_at, r.windows.five_hour.map(|w| w.pct)), ("acct-1", TS0_SECS, Some(12.0)));
    }

    #[test]
    fn only_the_tail_of_a_large_file_is_read() {
        let d = tempfile::tempdir().unwrap();
        let filler = format!(r#"{{"type":"response_item","payload":{{"text":"{}"}}}}"#, "x".repeat(4096));
        let mut lines = vec![meta_line("acct-1"), token_line("2027-01-15T07:00:00Z", "codex", 1.0, 1.0)];
        lines.extend(std::iter::repeat_n(filler.clone(), (TAIL_BYTES as usize * 2 / 4096) + 20));
        lines.push(token_line(TS0, "codex", 55.0, 3.0));
        lines.push(filler);
        let f = write_session(d.path(), "rollout-big.jsonl", &lines);
        let len = std::fs::metadata(&f).unwrap().len();
        assert!(len > TAIL_BYTES * 2);
        let r = read_session(&f, len).unwrap();
        assert_eq!(r.windows.five_hour.map(|w| w.pct), Some(55.0), "the newest line, found without reading the whole file");
    }

    #[test]
    fn a_file_with_no_creator_or_no_reading_gives_nothing() {
        let d = tempfile::tempdir().unwrap();
        let none = write_session(d.path(), "rollout-b.jsonl", &[meta_line("acct-1")]);
        assert!(read_session(&none, std::fs::metadata(&none).unwrap().len()).is_none());
        let headless = write_session(d.path(), "rollout-c.jsonl", &[token_line(TS0, "codex", 1.0, 1.0)]);
        assert!(read_session(&headless, std::fs::metadata(&headless).unwrap().len()).is_none());
        assert!(read_session(&d.path().join("missing.jsonl"), 0).is_none());
    }

    // ---- ingest ----

    use crate::config::Account;

    fn openai(id: &str, uuid: Option<&str>) -> Account {
        Account { id: id.into(), provider: Provider::Openai, label: id.into(), email: None, uuid: uuid.map(str::to_string) }
    }

    fn setup(accounts: Vec<Account>) -> (tempfile::TempDir, Paths, Config) {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let mut config = Config::default();
        for a in accounts {
            config.add_account(a).unwrap();
        }
        (d, paths, config)
    }

    fn day_dir(paths: &Paths) -> PathBuf {
        paths.codex_sessions_dir().join("2027/01/15")
    }

    #[test]
    fn ingest_credits_the_account_that_owns_the_creator_id() {
        let (_d, paths, config) = setup(vec![openai("work", Some("acct-w")), openai("home", Some("acct-h"))]);
        write_session(&day_dir(&paths), "rollout-1.jsonl", &[meta_line("acct-h"), token_line(TS0, "codex", 31.0, 6.0)]);
        let mut ing = CodexSessions::new();
        assert_eq!(ing.ingest(&paths, &config, T + 100).unwrap(), 1);
        let s = State::load(&paths).unwrap();
        assert_eq!(s.usage["home"].fetched_at, Some(TS0_SECS));
        assert_eq!(s.usage["home"].windows.weekly.map(|w| w.pct), Some(6.0));
        assert!(!s.usage.contains_key("work"));
    }

    #[test]
    fn ingest_ignores_unknown_creators_old_files_and_accounts_saved_without_an_id() {
        let (_d, paths, config) = setup(vec![openai("work", Some("acct-w")), openai("bare", None)]);
        write_session(&day_dir(&paths), "rollout-1.jsonl", &[meta_line("acct-stranger"), token_line(TS0, "codex", 31.0, 6.0)]);
        let old = write_session(&day_dir(&paths), "rollout-2.jsonl", &[meta_line("acct-w"), token_line(TS0, "codex", 31.0, 6.0)]);
        let long_ago = SystemTime::now() - std::time::Duration::from_secs(RECENT_S + 60);
        File::options().write(true).open(&old).unwrap().set_modified(long_ago).unwrap();
        assert_eq!(CodexSessions::new().ingest(&paths, &config, T + 100).unwrap(), 0);
        assert!(State::load(&paths).unwrap().usage.is_empty());
    }

    #[test]
    fn ingest_without_an_openai_account_or_a_session_folder_does_nothing() {
        let (_d, paths, config) = setup(vec![openai("work", Some("acct-w"))]);
        assert_eq!(CodexSessions::new().ingest(&paths, &config, T).unwrap(), 0);
        let (_d2, paths2, none) = setup(vec![]);
        write_session(&day_dir(&paths2), "rollout-1.jsonl", &[meta_line("acct-w"), token_line(TS0, "codex", 1.0, 1.0)]);
        assert_eq!(CodexSessions::new().ingest(&paths2, &none, T).unwrap(), 0);
    }

    #[test]
    fn ingest_takes_the_newest_reading_across_files_of_one_account() {
        let (_d, paths, config) = setup(vec![openai("work", Some("acct-w"))]);
        write_session(&day_dir(&paths), "rollout-1.jsonl", &[meta_line("acct-w"), token_line("2027-01-15T07:00:00Z", "codex", 10.0, 1.0)]);
        write_session(&day_dir(&paths), "rollout-2.jsonl", &[meta_line("acct-w"), token_line(TS0, "codex", 20.0, 2.0)]);
        CodexSessions::new().ingest(&paths, &config, T + 100).unwrap();
        assert_eq!(State::load(&paths).unwrap().usage["work"].windows.five_hour.map(|w| w.pct), Some(20.0));
    }

    #[test]
    fn ingest_never_stamps_a_reading_later_than_now() {
        let (_d, paths, config) = setup(vec![openai("work", Some("acct-w"))]);
        write_session(&day_dir(&paths), "rollout-1.jsonl", &[meta_line("acct-w"), token_line("2037-01-15T08:00:00Z", "codex", 10.0, 1.0)]);
        CodexSessions::new().ingest(&paths, &config, T).unwrap();
        assert_eq!(State::load(&paths).unwrap().usage["work"].fetched_at, Some(T));
    }

    #[test]
    fn ingest_does_not_reread_a_file_that_did_not_change() {
        let (_d, paths, config) = setup(vec![openai("work", Some("acct-w"))]);
        let f = write_session(&day_dir(&paths), "rollout-1.jsonl", &[meta_line("acct-w"), token_line(TS0, "codex", 10.0, 1.0)]);
        let mut ing = CodexSessions::new();
        assert_eq!(ing.ingest(&paths, &config, T + 100).unwrap(), 1);
        // Wipe the state: an unchanged file is skipped, so nothing comes back.
        std::fs::remove_file(paths.state_file()).unwrap();
        assert_eq!(ing.ingest(&paths, &config, T + 200).unwrap(), 0);
        // Appending a line makes it a new file again.
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str(&token_line("2027-01-15T08:00:30Z", "codex", 11.0, 1.0));
        text.push('\n');
        std::fs::write(&f, text).unwrap();
        assert_eq!(ing.ingest(&paths, &config, T + 300).unwrap(), 1);
        assert_eq!(State::load(&paths).unwrap().usage["work"].windows.five_hour.map(|w| w.pct), Some(11.0));
    }

    #[test]
    fn two_saved_logins_of_one_workspace_go_to_the_one_active_on_a_codex_surface() {
        let (_d, paths, config) = setup(vec![openai("one", Some("acct-w")), openai("two", Some("acct-w"))]);
        write_session(&day_dir(&paths), "rollout-1.jsonl", &[meta_line("acct-w"), token_line(TS0, "codex", 10.0, 1.0)]);
        let mut ing = CodexSessions::new();
        assert_eq!(ing.ingest(&paths, &config, T + 100).unwrap(), 0, "ambiguous: nobody is credited");
        State::update(&paths, |s| s.set_active(SurfaceKind::CodexCli, Some("two".into()))).unwrap();
        assert_eq!(CodexSessions::new().ingest(&paths, &config, T + 100).unwrap(), 1);
        let s = State::load(&paths).unwrap();
        assert!(s.usage.contains_key("two") && !s.usage.contains_key("one"));
    }
}
