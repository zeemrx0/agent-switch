//! Claude Code's status line as a source of live quota readings.
//!
//! Claude Code runs the `statusLine` command from `~/.claude/settings.json` after each API response and
//! writes a JSON document to its stdin. Since 2.1.x the document holds
//! `rate_limits.five_hour` and `rate_limits.seven_day`, each `{"used_percentage": <0..100>, "resets_at":
//! <Unix seconds>}` (the percentage is the `anthropic-ratelimit-unified-*-utilization` header times 100, the
//! reset time is the matching `*-reset` header). A window is absent when the plan does not have it.
//!
//! `agent-switch statusline install` makes that command `agent-switch statusline`, which takes the reading and
//! then runs the user's original command, so the visible status line does not change. The wrapper must never
//! break the status line: it prints nothing of its own, ignores its own errors and exits with the original
//! command's code. It uses no network and no Keychain.
//!
//! The reading belongs to the account active on `claude-cli`. A running session follows a switch within about
//! 30 seconds, so readings are skipped for `SWITCH_GRACE_S` after one rather than credited to the wrong account.

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Map, Value};

use crate::adapters::codex::backup_file;
use crate::config::{Config, SurfaceKind};
use crate::error::{Error, Result};
use crate::fsutil::{safe_write, WriteOpts};
use crate::live_usage;
use crate::paths::Paths;
use crate::state::{State, UsageRecord, UsageState, WindowSnap, Windows};

/// Readings are not credited to the active account this long after a switch of `claude-cli`.
pub const SWITCH_GRACE_S: u64 = 60;

/// The words after the binary path in the installed command.
const SUBCOMMAND: &str = "statusline";
const KEY: &str = "statusLine";

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// The windows in Claude Code's status line input, or `None` when it has none (not JSON, no `rate_limits`,
/// or a plan without limits). A window with an unusable percentage is left out.
pub fn parse_input(input: &str) -> Option<Windows> {
    let v: Value = serde_json::from_str(input).ok()?;
    let limits = v.get("rate_limits")?;
    let window = |key: &str| -> Option<WindowSnap> {
        let w = limits.get(key)?;
        let pct = w.get("used_percentage")?.as_f64().filter(|p| p.is_finite() && *p >= 0.0)?;
        Some(WindowSnap { pct, resets_at: w.get("resets_at").and_then(Value::as_i64) })
    };
    let windows = Windows { five_hour: window("five_hour"), weekly: window("seven_day") };
    (windows.five_hour.is_some() || windows.weekly.is_some()).then_some(windows)
}

/// The account a reading taken at `now` belongs to: the one active on `claude-cli`, when that surface is
/// managed, the account is saved, and no switch happened within `SWITCH_GRACE_S`.
pub fn attribute<'a>(config: &Config, state: &'a State, now: u64) -> Option<&'a str> {
    config.surfaces.get(SurfaceKind::ClaudeCli).account_id()?;
    let active = state.active(SurfaceKind::ClaudeCli)?;
    config.account(active)?;
    let last_switch = state.journal.iter().rev().find(|r| r.surface == SurfaceKind::ClaudeCli).map(|r| r.at);
    if last_switch.is_some_and(|at| now.saturating_sub(at) < SWITCH_GRACE_S) {
        return None;
    }
    Some(active)
}

/// Two reset times this close are the same window (the servers' reset stamps drift by a few seconds).
const SAME_WINDOW_S: i64 = 300;

/// The input carries no observation time, so it is stamped with the time of the call. An idle session that
/// redraws its status line repeats the `rate_limits` of its last response, possibly minutes old, while another
/// session has already read a newer value. Usage only rises inside a window, so a reading below the stored
/// one in the same window is stale. A window counts as the same when both reset times are known, differ by at
/// most `SAME_WINDOW_S`, and the stored one has not passed. A stored record that is not `Ok` never blocks.
pub fn is_stale(stored: &UsageRecord, new: &Windows, now: u64) -> bool {
    if stored.state != UsageState::Ok {
        return false;
    }
    let lower = |old: Option<WindowSnap>, new: Option<WindowSnap>| match (old, new) {
        (Some(o), Some(n)) => match (o.resets_at, n.resets_at) {
            (Some(or), Some(nr)) => or > now as i64 && (or - nr).abs() <= SAME_WINDOW_S && o.pct > n.pct,
            _ => false,
        },
        _ => false,
    };
    lower(stored.windows.five_hour, new.five_hour) || lower(stored.windows.weekly, new.weekly)
}

/// Take the reading in `input`, if there is one, it is not stale (`is_stale`) and there is an account to
/// credit it to. Returns whether the state changed.
pub fn record_input(paths: &Paths, input: &str, now: u64) -> Result<bool> {
    let Some(windows) = parse_input(input) else { return Ok(false) };
    let config = Config::load(paths)?;
    let state = State::load(paths)?;
    let Some(account) = attribute(&config, &state, now) else { return Ok(false) };
    if state.usage.get(account).is_some_and(|stored| is_stale(stored, &windows, now)) {
        return Ok(false);
    }
    live_usage::record(paths, account, windows, now)
}

// ---------------------------------------------------------------------------------------------
// The wrapper
// ---------------------------------------------------------------------------------------------

/// The saved record: the `statusLine` that was in the settings before the wrapper, `None` when there was none.
fn load_saved(paths: &Paths) -> Option<Value> {
    let text = std::fs::read_to_string(paths.statusline_saved_file()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("original").cloned().filter(|o| !o.is_null())
}

/// The command to run in place of the wrapper: the saved `statusLine.command`, when it is a command.
fn original_command(paths: &Paths) -> Option<String> {
    let saved = load_saved(paths)?;
    if saved.get("type").and_then(Value::as_str).is_some_and(|t| t != "command") {
        return None;
    }
    saved.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()).map(str::to_string)
}

/// Run `command` with `/bin/sh -c`, feed it `input`, and return its exit code. Its stdout and stderr go
/// straight through. A command that cannot start counts as exit code 127.
fn run_original(command: &str, input: &[u8]) -> i32 {
    let Ok(mut child) = Command::new("/bin/sh").arg("-c").arg(command).stdin(Stdio::piped()).spawn() else { return 127 };
    // Feed stdin from a thread, so a command that writes a lot before it reads cannot stall on a full pipe.
    let feeder = child.stdin.take().map(|mut stdin| {
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        })
    });
    let status = child.wait();
    if let Some(f) = feeder {
        let _ = f.join();
    }
    match status {
        Ok(s) => s.code().or_else(|| s.signal().map(|n| 128 + n)).unwrap_or(1),
        Err(_) => 1,
    }
}

/// What `agent-switch statusline` does: take the reading, then run the original command with the same input.
/// Returns the process exit code. Reports nothing of its own, whatever goes wrong.
pub fn run(paths: Option<&Paths>, input: &[u8], now: u64) -> i32 {
    let Some(paths) = paths else { return 0 };
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = record_input(paths, text, now);
    }
    match original_command(paths) {
        Some(command) => run_original(&command, input),
        None => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Install and uninstall
// ---------------------------------------------------------------------------------------------

/// Single-quote `path` for `/bin/sh` unless it is made of characters that need none.
fn shell_quote(path: &str) -> String {
    if !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+@:,".contains(c)) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// The `statusLine.command` that runs `binary`.
pub fn wrapper_command(binary: &Path) -> String {
    format!("{} {SUBCOMMAND}", shell_quote(&binary.display().to_string()))
}

/// True when `command` is a wrapper command of any agent-switch binary path.
pub fn is_wrapper(command: &str) -> bool {
    let Some(binary) = command.trim_end().strip_suffix(&format!(" {SUBCOMMAND}")) else { return false };
    let binary = binary.trim_matches('\'');
    binary == "agent-switch" || binary.ends_with("/agent-switch")
}

fn statusline_command(settings: &Map<String, Value>) -> Option<&str> {
    settings.get(KEY)?.get("command")?.as_str()
}

fn read_settings(paths: &Paths) -> Result<Option<(Map<String, Value>, String)>> {
    let file = paths.claude_settings_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("read {}", file.display()), e)),
    };
    if text.trim().is_empty() {
        return Ok(Some((Map::new(), text)));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(Some((map, text))),
        _ => Err(Error::Invalid(format!("{} is not a JSON object; fix or remove it first", file.display()))),
    }
}

/// Write the settings back with 2-space indentation (Claude Code's own style), keeping the file's mode and
/// its final newline.
fn write_settings(paths: &Paths, settings: Map<String, Value>, previous: Option<&str>) -> Result<()> {
    let file = paths.claude_settings_file();
    let mut text = serde_json::to_string_pretty(&Value::Object(settings)).map_err(|e| Error::Invalid(e.to_string()))?;
    if previous.is_none_or(|p| p.ends_with('\n')) {
        text.push('\n');
    }
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&file).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600)
    };
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(format!("create {}", dir.display()), e))?;
    }
    safe_write(&file, text.as_bytes(), &WriteOpts::secret().with_mode(mode))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Install {
    /// The wrapper is now set. `backup` is the copy of `settings.json` made first, when there was a file.
    Installed { backup: Option<PathBuf> },
    /// The wrapper was set already, for another agent-switch binary path; the path was updated.
    Repointed { backup: Option<PathBuf> },
    AlreadyInstalled,
}

/// Wrap the `statusLine` of `~/.claude/settings.json` so `binary statusline` runs first and then the original
/// command. Idempotent: a second call never wraps the wrapper, and never replaces the saved original.
pub fn install(paths: &Paths, binary: &Path) -> Result<Install> {
    let command = wrapper_command(binary);
    let settings = read_settings(paths)?;
    let previous = settings.as_ref().map(|(_, t)| t.clone());
    let mut settings = settings.map(|(m, _)| m).unwrap_or_default();

    if let Some(current) = statusline_command(&settings).filter(|c| is_wrapper(c)) {
        if current == command {
            return Ok(Install::AlreadyInstalled);
        }
        let backup = backup_settings(paths)?;
        if let Some(Value::Object(line)) = settings.get_mut(KEY) {
            line.insert("command".into(), Value::String(command));
        }
        write_settings(paths, settings, previous.as_deref())?;
        return Ok(Install::Repointed { backup });
    }

    let original = settings.get(KEY).cloned();
    let backup = backup_settings(paths)?;
    // Saved before the settings change, so a failure in between leaves the settings as they were.
    let saved = serde_json::to_string_pretty(&json!({ "original": original })).map_err(|e| Error::Invalid(e.to_string()))?;
    crate::fsutil::ensure_private_dir(&paths.support_dir())?;
    safe_write(&paths.statusline_saved_file(), saved.as_bytes(), &WriteOpts::secret())?;

    let mut line = match original {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    line.insert("type".into(), Value::String("command".into()));
    line.insert("command".into(), Value::String(command));
    settings.insert(KEY.into(), Value::Object(line));
    write_settings(paths, settings, previous.as_deref())?;
    Ok(Install::Installed { backup })
}

fn backup_settings(paths: &Paths) -> Result<Option<PathBuf>> {
    let file = paths.claude_settings_file();
    if file.exists() { backup_file(paths, &file, "claude-settings").map(Some) } else { Ok(None) }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Uninstall {
    Restored,
    /// The settings do not hold the wrapper (never installed, or the user changed the line since): untouched.
    NotInstalled,
}

/// Put the saved `statusLine` back exactly, or remove the key when there was none.
pub fn uninstall(paths: &Paths) -> Result<Uninstall> {
    let Some((mut settings, text)) = read_settings(paths)? else { return Ok(Uninstall::NotInstalled) };
    if !statusline_command(&settings).is_some_and(is_wrapper) {
        return Ok(Uninstall::NotInstalled);
    }
    let saved_file = paths.statusline_saved_file();
    let saved = match std::fs::read_to_string(&saved_file) {
        Ok(t) => serde_json::from_str::<Value>(&t).map_err(|e| Error::Invalid(format!("{}: {e}", saved_file.display())))?,
        Err(e) => return Err(Error::io(format!("read {} (the original status line is not saved; edit settings.json by hand)", saved_file.display()), e)),
    };
    match saved.get("original").filter(|o| !o.is_null()) {
        Some(original) => {
            settings.insert(KEY.into(), original.clone());
        }
        None => {
            settings.shift_remove(KEY);
        }
    }
    write_settings(paths, settings, Some(&text))?;
    match std::fs::remove_file(&saved_file) {
        Ok(()) => Ok(Uninstall::Restored),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Uninstall::Restored),
        Err(e) => Err(Error::io(format!("remove {}", saved_file.display()), e)),
    }
}

/// What `doctor` reports.
#[derive(Debug, PartialEq, Eq)]
pub enum Inspection {
    NotInstalled,
    /// Installed, and the binary it runs exists and the original is saved.
    Installed,
    /// Installed but broken: the text says what to do.
    Broken(String),
}

pub fn inspect(paths: &Paths) -> Result<Inspection> {
    let Some((settings, _)) = read_settings(paths)? else { return Ok(Inspection::NotInstalled) };
    let Some(command) = statusline_command(&settings).filter(|c| is_wrapper(c)) else { return Ok(Inspection::NotInstalled) };
    let binary = command.trim_end().strip_suffix(&format!(" {SUBCOMMAND}")).unwrap_or(command).trim_matches('\'');
    if !Path::new(binary).is_file() {
        return Ok(Inspection::Broken(format!("the status line runs {binary}, which does not exist; run `agent-switch statusline install` from the current binary")));
    }
    if !paths.statusline_saved_file().is_file() {
        return Ok(Inspection::Broken("the saved original status line is missing, so `statusline uninstall` cannot restore it".into()));
    }
    Ok(Inspection::Installed)
}

#[cfg(test)]
mod tests;
