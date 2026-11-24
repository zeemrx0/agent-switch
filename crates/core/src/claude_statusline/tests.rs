use super::*;
use crate::config::{Account, Binding, Provider};

const NOW: u64 = 1_800_000_000;

const INPUT: &str = r#"{"session_id":"s","model":{"id":"m"},"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1800003600},"seven_day":{"used_percentage":41.2,"resets_at":1800400000}}}"#;

fn home() -> (tempfile::TempDir, Paths) {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::from_home(d.path());
    (d, paths)
}

/// Account `a` is saved, bound to claude-cli and active there since long ago.
fn managed(paths: &Paths) {
    let mut c = Config::default();
    c.add_account(Account { id: "a".into(), provider: Provider::Claude, label: "A".into(), email: None, uuid: None }).unwrap();
    c.bind(SurfaceKind::ClaudeCli, Binding::parse("a")).unwrap();
    c.save(paths).unwrap();
    State::update(paths, |s| s.record_switch(SurfaceKind::ClaudeCli, "a", NOW - 3600)).unwrap();
}

fn write_settings_text(paths: &Paths, text: &str) {
    let f = paths.claude_settings_file();
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(f, text).unwrap();
}

fn settings_text(paths: &Paths) -> String {
    std::fs::read_to_string(paths.claude_settings_file()).unwrap()
}

#[test]
fn the_input_gives_both_windows_with_percent_and_reset_seconds() {
    let w = parse_input(INPUT).unwrap();
    assert_eq!(w.five_hour, Some(WindowSnap { pct: 23.5, resets_at: Some(1_800_003_600) }));
    assert_eq!(w.weekly, Some(WindowSnap { pct: 41.2, resets_at: Some(1_800_400_000) }));
}

#[test]
fn an_input_with_one_window_or_no_reset_time_still_gives_a_reading() {
    let w = parse_input(r#"{"rate_limits":{"seven_day":{"used_percentage":7}}}"#).unwrap();
    assert_eq!((w.five_hour, w.weekly), (None, Some(WindowSnap { pct: 7.0, resets_at: None })));
}

#[test]
fn an_input_without_usable_limits_gives_nothing() {
    for bad in [
        "",
        "not json",
        "[]",
        r#"{"model":{"id":"m"}}"#,
        r#"{"rate_limits":{}}"#,
        r#"{"rate_limits":null}"#,
        r#"{"rate_limits":{"five_hour":{"used_percentage":null}}}"#,
        r#"{"rate_limits":{"five_hour":{"used_percentage":"12"}}}"#,
        r#"{"rate_limits":{"five_hour":{"used_percentage":-3}}}"#,
        r#"{"rate_limits":{"five_hour":{"resets_at":5}}}"#,
    ] {
        assert!(parse_input(bad).is_none(), "{bad}");
    }
}

#[test]
fn a_reading_goes_to_the_account_active_on_claude_cli() {
    let (_d, paths) = home();
    managed(&paths);
    assert!(record_input(&paths, INPUT, NOW).unwrap());
    let rec = &State::load(&paths).unwrap().usage["a"];
    assert_eq!((rec.fetched_at, rec.windows.five_hour.map(|w| w.pct)), (Some(NOW), Some(23.5)));
}

#[test]
fn a_reading_is_skipped_when_claude_cli_is_unmanaged_or_has_no_active_account() {
    let (_d, paths) = home();
    managed(&paths);
    let mut c = Config::load(&paths).unwrap();
    c.bind(SurfaceKind::ClaudeCli, Binding::Unmanaged).unwrap();
    c.save(&paths).unwrap();
    assert!(!record_input(&paths, INPUT, NOW).unwrap());

    let (_d2, paths2) = home();
    managed(&paths2);
    State::update(&paths2, |s| s.set_active(SurfaceKind::ClaudeCli, None)).unwrap();
    assert!(!record_input(&paths2, INPUT, NOW).unwrap());

    let (_d3, paths3) = home();
    assert!(!record_input(&paths3, INPUT, NOW).unwrap(), "no config at all");
    assert!(State::load(&paths3).unwrap().usage.is_empty());
}

#[test]
fn a_reading_is_skipped_for_a_minute_after_a_switch() {
    let (_d, paths) = home();
    managed(&paths);
    State::update(&paths, |s| s.record_switch(SurfaceKind::ClaudeCli, "a", NOW - 30)).unwrap();
    assert!(!record_input(&paths, INPUT, NOW).unwrap());
    assert!(record_input(&paths, INPUT, NOW - 30 + SWITCH_GRACE_S).unwrap());
}

#[test]
fn a_switch_of_another_surface_does_not_hold_readings_back() {
    let (_d, paths) = home();
    managed(&paths);
    State::update(&paths, |s| s.record_switch(SurfaceKind::CodexCli, "x", NOW - 5)).unwrap();
    assert!(record_input(&paths, INPUT, NOW).unwrap());
}

#[test]
fn the_wrapper_command_quotes_a_path_that_needs_it_and_is_recognised_either_way() {
    assert_eq!(wrapper_command(Path::new("/opt/homebrew/bin/agent-switch")), "/opt/homebrew/bin/agent-switch statusline");
    let spaced = wrapper_command(Path::new("/Users/me/My Tools/agent-switch"));
    assert_eq!(spaced, "'/Users/me/My Tools/agent-switch' statusline");
    assert!(is_wrapper(&spaced) && is_wrapper("/opt/homebrew/bin/agent-switch statusline"));
    assert!(!is_wrapper("~/bin/other-tool statusline") && !is_wrapper("/x/agent-switch list") && !is_wrapper("echo hi"));
}

const ORIGINAL: &str = r##"{
  "model": "opus",
  "statusLine": {
    "type": "command",
    "command": "orca-statusline --flag",
    "padding": 2
  },
  "env": {
    "A": "1"
  }
}
"##;

#[test]
fn install_wraps_the_status_line_and_uninstall_restores_it_exactly() {
    let (_d, paths) = home();
    write_settings_text(&paths, ORIGINAL);
    let bin = Path::new("/opt/bin/agent-switch");

    let Install::Installed { backup: Some(backup) } = install(&paths, bin).unwrap() else { panic!("expected a fresh install with a backup") };
    assert_eq!(std::fs::read_to_string(backup).unwrap(), ORIGINAL);
    let v: Value = serde_json::from_str(&settings_text(&paths)).unwrap();
    assert_eq!(v["statusLine"]["command"], "/opt/bin/agent-switch statusline");
    assert_eq!((&v["statusLine"]["type"], &v["statusLine"]["padding"]), (&json!("command"), &json!(2)));
    assert_eq!((&v["model"], &v["env"]), (&json!("opus"), &json!({"A": "1"})), "other keys survive");
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["model", "statusLine", "env"], "key order survives");

    assert_eq!(uninstall(&paths).unwrap(), Uninstall::Restored);
    assert_eq!(settings_text(&paths), ORIGINAL);
    assert!(!paths.statusline_saved_file().exists());
}

#[test]
fn install_into_settings_without_a_status_line_adds_one_and_uninstall_removes_the_key() {
    let (_d, paths) = home();
    write_settings_text(&paths, "{\n  \"model\": \"opus\"\n}\n");
    install(&paths, Path::new("/opt/bin/agent-switch")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&settings_text(&paths)).unwrap()["statusLine"]["type"], "command");
    assert_eq!(uninstall(&paths).unwrap(), Uninstall::Restored);
    assert_eq!(settings_text(&paths), "{\n  \"model\": \"opus\"\n}\n");
}

#[test]
fn install_without_a_settings_file_creates_it_and_uninstall_leaves_an_empty_object() {
    let (_d, paths) = home();
    assert_eq!(install(&paths, Path::new("/opt/bin/agent-switch")).unwrap(), Install::Installed { backup: None });
    assert!(settings_text(&paths).contains("/opt/bin/agent-switch statusline"));
    uninstall(&paths).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&settings_text(&paths)).unwrap(), json!({}));
}

#[test]
fn a_second_install_changes_nothing_and_keeps_the_saved_original() {
    let (_d, paths) = home();
    write_settings_text(&paths, ORIGINAL);
    let bin = Path::new("/opt/bin/agent-switch");
    install(&paths, bin).unwrap();
    let after_first = settings_text(&paths);
    let saved = std::fs::read_to_string(paths.statusline_saved_file()).unwrap();
    assert_eq!(install(&paths, bin).unwrap(), Install::AlreadyInstalled);
    assert_eq!(settings_text(&paths), after_first);
    assert_eq!(std::fs::read_to_string(paths.statusline_saved_file()).unwrap(), saved);
    assert_eq!(std::fs::read_dir(paths.support_dir().join("backups")).unwrap().count(), 1, "only the first install backs up");
}

#[test]
fn install_from_a_moved_binary_repoints_the_wrapper_without_wrapping_it() {
    let (_d, paths) = home();
    write_settings_text(&paths, ORIGINAL);
    install(&paths, Path::new("/old/agent-switch")).unwrap();
    assert!(matches!(install(&paths, Path::new("/new/agent-switch")).unwrap(), Install::Repointed { .. }));
    let v: Value = serde_json::from_str(&settings_text(&paths)).unwrap();
    assert_eq!(v["statusLine"]["command"], "/new/agent-switch statusline");
    uninstall(&paths).unwrap();
    assert_eq!(settings_text(&paths), ORIGINAL);
}

#[test]
fn uninstall_leaves_a_status_line_that_is_not_the_wrapper_alone() {
    let (_d, paths) = home();
    assert_eq!(uninstall(&paths).unwrap(), Uninstall::NotInstalled);
    write_settings_text(&paths, ORIGINAL);
    assert_eq!(uninstall(&paths).unwrap(), Uninstall::NotInstalled);
    assert_eq!(settings_text(&paths), ORIGINAL);
}

#[test]
fn a_settings_file_that_is_not_a_json_object_is_never_overwritten() {
    let (_d, paths) = home();
    for bad in ["{not json", "[1]"] {
        write_settings_text(&paths, bad);
        assert!(install(&paths, Path::new("/opt/bin/agent-switch")).is_err());
        assert_eq!(settings_text(&paths), bad);
    }
    assert!(!paths.statusline_saved_file().exists());
}

#[test]
fn inspect_tells_installed_from_missing_and_broken() {
    let (d, paths) = home();
    assert_eq!(inspect(&paths).unwrap(), Inspection::NotInstalled);
    let bin = d.path().join("agent-switch");
    std::fs::write(&bin, "").unwrap();
    install(&paths, &bin).unwrap();
    assert_eq!(inspect(&paths).unwrap(), Inspection::Installed);
    std::fs::remove_file(paths.statusline_saved_file()).unwrap();
    assert!(matches!(inspect(&paths).unwrap(), Inspection::Broken(m) if m.contains("saved original")));
    std::fs::remove_file(&bin).unwrap();
    assert!(matches!(inspect(&paths).unwrap(), Inspection::Broken(m) if m.contains("does not exist")));
}

fn save_original(paths: &Paths, original: Value) {
    std::fs::create_dir_all(paths.support_dir()).unwrap();
    std::fs::write(paths.statusline_saved_file(), json!({ "original": original }).to_string()).unwrap();
}

#[test]
fn run_feeds_the_original_the_same_input_and_returns_its_exit_code() {
    let (d, paths) = home();
    managed(&paths);
    let seen = d.path().join("stdin.txt");
    save_original(&paths, json!({"type": "command", "command": format!("cat > '{}'; exit 3", seen.display())}));
    assert_eq!(run(Some(&paths), INPUT.as_bytes(), NOW), 3);
    assert_eq!(std::fs::read_to_string(seen).unwrap(), INPUT);
    assert_eq!(State::load(&paths).unwrap().usage["a"].fetched_at, Some(NOW), "the reading was taken too");
}

#[test]
fn run_still_runs_the_original_when_the_input_is_garbage_or_agent_switch_has_no_state() {
    let (d, paths) = home();
    let seen = d.path().join("stdin.txt");
    save_original(&paths, json!({"type": "command", "command": format!("cat > '{}'; exit 0", seen.display())}));
    assert_eq!(run(Some(&paths), b"\xff\xfe not json", NOW), 0);
    assert_eq!(std::fs::read(&seen).unwrap(), b"\xff\xfe not json");
    assert!(!paths.state_file().exists(), "nothing was written for an unreadable input");
}

#[test]
fn run_survives_a_corrupt_state_file_and_a_large_input_the_original_ignores() {
    let (_d, paths) = home();
    managed(&paths);
    std::fs::write(paths.state_file(), "{broken").unwrap();
    save_original(&paths, json!({"type": "command", "command": "exit 7"}));
    let big = format!(r#"{{"pad":"{}"}}"#, "x".repeat(1 << 20));
    assert_eq!(run(Some(&paths), big.as_bytes(), NOW), 7);
}

#[test]
fn run_has_nothing_to_run_without_a_saved_command() {
    let (_d, paths) = home();
    managed(&paths);
    assert_eq!(run(Some(&paths), INPUT.as_bytes(), NOW), 0);
    assert_eq!(run(None, INPUT.as_bytes(), NOW), 0);
    save_original(&paths, json!({"type": "command", "command": "   "}));
    assert_eq!(run(Some(&paths), INPUT.as_bytes(), NOW), 0);
    save_original(&paths, Value::Null);
    assert_eq!(run(Some(&paths), INPUT.as_bytes(), NOW), 0);
}

#[test]
fn run_passes_a_failing_or_unstartable_original_through() {
    let (_d, paths) = home();
    save_original(&paths, json!({"type": "command", "command": "/nonexistent/tool"}));
    assert_eq!(run(Some(&paths), b"{}", NOW), 127);
    save_original(&paths, json!({"type": "command", "command": "kill -9 $$"}));
    assert_eq!(run(Some(&paths), b"{}", NOW), 137);
}

/// Stored: five-hour at 40 % resetting at NOW + 3600 (the same reset time `INPUT` carries), fetched a while ago.
fn store_reading(paths: &Paths, state: UsageState, five_pct: f64, resets_at: i64) {
    State::update(paths, |s| {
        let rec = s.usage.entry("a".into()).or_default();
        rec.state = state;
        rec.windows = Windows { five_hour: Some(WindowSnap { pct: five_pct, resets_at: Some(resets_at) }), weekly: None };
        rec.fetched_at = Some(NOW - 120);
    })
    .unwrap();
}

fn five_hour_input(pct: f64, resets_at: i64) -> String {
    format!(r#"{{"rate_limits":{{"five_hour":{{"used_percentage":{pct},"resets_at":{resets_at}}}}}}}"#)
}

fn stored_pct(paths: &Paths) -> Option<f64> {
    State::load(paths).unwrap().usage["a"].windows.five_hour.map(|w| w.pct)
}

#[test]
fn a_lower_percent_in_the_same_window_is_ignored_as_stale() {
    let (_d, paths) = home();
    managed(&paths);
    let reset = NOW as i64 + 3600;
    store_reading(&paths, UsageState::Ok, 40.0, reset);
    assert!(!record_input(&paths, &five_hour_input(35.0, reset), NOW).unwrap());
    assert_eq!(stored_pct(&paths), Some(40.0));
    assert_eq!(State::load(&paths).unwrap().usage["a"].fetched_at, Some(NOW - 120));
}

#[test]
fn a_higher_percent_in_the_same_window_is_accepted() {
    let (_d, paths) = home();
    managed(&paths);
    let reset = NOW as i64 + 3600;
    store_reading(&paths, UsageState::Ok, 40.0, reset);
    assert!(record_input(&paths, &five_hour_input(41.0, reset), NOW).unwrap());
    assert_eq!(stored_pct(&paths), Some(41.0));
}

#[test]
fn a_lower_percent_in_a_new_window_is_accepted() {
    let (_d, paths) = home();
    managed(&paths);
    store_reading(&paths, UsageState::Ok, 90.0, NOW as i64 + 3600);
    assert!(record_input(&paths, &five_hour_input(3.0, NOW as i64 + 3600 + 5 * 3600), NOW).unwrap(), "different reset time");
    assert_eq!(stored_pct(&paths), Some(3.0));

    // The stored window has already ended: whatever the new reset time says, the window rolled over.
    store_reading(&paths, UsageState::Ok, 90.0, NOW as i64 - 10);
    assert!(record_input(&paths, &five_hour_input(2.0, NOW as i64 - 5), NOW + 1).unwrap());
    assert_eq!(stored_pct(&paths), Some(2.0));
}

#[test]
fn reset_times_a_few_minutes_apart_are_still_the_same_window() {
    let (_d, paths) = home();
    managed(&paths);
    let reset = NOW as i64 + 3600;
    store_reading(&paths, UsageState::Ok, 40.0, reset);
    assert!(!record_input(&paths, &five_hour_input(35.0, reset + 299), NOW).unwrap());
    assert!(!record_input(&paths, &five_hour_input(35.0, reset - 299), NOW).unwrap());
    assert!(record_input(&paths, &five_hour_input(35.0, reset + 301), NOW).unwrap(), "301 s apart is a new window");
}

#[test]
fn a_stored_record_that_is_not_ok_or_has_no_windows_does_not_block() {
    let (_d, paths) = home();
    managed(&paths);
    let reset = NOW as i64 + 3600;
    store_reading(&paths, UsageState::Unreadable, 40.0, reset);
    assert!(record_input(&paths, &five_hour_input(35.0, reset), NOW).unwrap());

    let rec = UsageRecord { state: UsageState::Ok, ..UsageRecord::default() };
    assert!(!is_stale(&rec, &parse_input(&five_hour_input(1.0, reset)).unwrap(), NOW), "no stored windows");
}

#[test]
fn either_window_being_lower_makes_the_reading_stale() {
    let rec = UsageRecord {
        state: UsageState::Ok,
        windows: Windows {
            five_hour: Some(WindowSnap { pct: 10.0, resets_at: Some(NOW as i64 + 3600) }),
            weekly: Some(WindowSnap { pct: 50.0, resets_at: Some(NOW as i64 + 86_400) }),
        },
        ..UsageRecord::default()
    };
    let input = |five: f64, week: f64| {
        parse_input(&format!(
            r#"{{"rate_limits":{{"five_hour":{{"used_percentage":{five},"resets_at":{}}},"seven_day":{{"used_percentage":{week},"resets_at":{}}}}}}}"#,
            NOW + 3600,
            NOW + 86_400
        ))
        .unwrap()
    };
    assert!(is_stale(&rec, &input(12.0, 49.0), NOW));
    assert!(is_stale(&rec, &input(9.0, 51.0), NOW));
    assert!(!is_stale(&rec, &input(10.0, 50.0), NOW), "equal is not lower");
}
