//! The status line wrapper end to end, against a temporary HOME: installing it around an existing status line,
//! passing the original's output and exit code through, taking the quota reading, and restoring the line.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use agent_switch_core::config::{Account, Binding, Config, Provider, SurfaceKind};
use agent_switch_core::paths::Paths;
use agent_switch_core::state::State;

const BIN: &str = env!("CARGO_BIN_EXE_agent-switch");

const INPUT: &str = r#"{"model":{"id":"m"},"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1800003600},"seven_day":{"used_percentage":41.2,"resets_at":1800400000}}}"#;

/// Run `program` with a temporary HOME, `stdin` as input, and nothing else from the real environment.
fn run_program(program: &str, args: &[&str], home: &Path, stdin: &str) -> Output {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("PATH", "/usr/bin:/bin");
    // Keep `doctor` off the real Keychain items of Claude Code and agy.
    cmd.env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/nonexistent/agent-switch-test");
    cmd.env("AGENT_SWITCH_ANTIGRAVITY_SERVICE", "agent-switch-test-nonexistent");
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("run the program");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn run(home: &Path, args: &[&str], stdin: &str) -> Output {
    run_program(BIN, args, home, stdin)
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn settings(home: &Path) -> std::path::PathBuf {
    home.join(".claude/settings.json")
}

fn write_settings(home: &Path, body: &str) {
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(settings(home), body).unwrap();
}

/// Account `a` is bound to claude-cli and has been active there for an hour.
fn manage_claude_cli(home: &Path) {
    let paths = Paths::from_home(home);
    let mut c = Config::default();
    c.add_account(Account { id: "a".into(), provider: Provider::Claude, label: "A".into(), email: None, uuid: None }).unwrap();
    c.bind(SurfaceKind::ClaudeCli, Binding::parse("a")).unwrap();
    c.save(&paths).unwrap();
    let hour_ago = agent_switch_core::state::now_secs() - 3600;
    State::update(&paths, |s| s.record_switch(SurfaceKind::ClaudeCli, "a", hour_ago)).unwrap();
}

const ORIGINAL: &str = "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"cat >/dev/null; printf 'my line'; exit 4\"\n  }\n}\n";

#[test]
fn the_installed_wrapper_shows_the_original_line_keeps_its_exit_code_and_takes_the_reading() {
    let home = tempfile::tempdir().unwrap();
    manage_claude_cli(home.path());
    write_settings(home.path(), ORIGINAL);

    let o = run(home.path(), &["statusline", "install"], "");
    assert!(o.status.success(), "{}", text(&o));
    let installed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(settings(home.path())).unwrap()).unwrap();
    let command = installed["statusLine"]["command"].as_str().unwrap();
    assert!(command.ends_with("agent-switch statusline") && Path::new(command.split(' ').next().unwrap()).is_absolute(), "{command}");

    // Run the installed command the way Claude Code does: through the shell, input on stdin.
    let o = run_program("/bin/sh", &["-c", command], home.path(), INPUT);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "my line");
    assert_eq!(o.status.code(), Some(4));
    assert!(o.stderr.is_empty(), "{}", text(&o));

    let state = State::load(&Paths::from_home(home.path())).unwrap();
    assert_eq!(state.usage["a"].windows.five_hour.map(|w| w.pct), Some(23.5));
    assert_eq!(state.usage["a"].windows.weekly.map(|w| w.pct), Some(41.2));

    let o = run(home.path(), &["statusline", "uninstall"], "");
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(std::fs::read_to_string(settings(home.path())).unwrap(), ORIGINAL);
}

#[test]
fn the_wrapper_is_silent_on_bad_input_and_with_nothing_to_wrap() {
    let home = tempfile::tempdir().unwrap();
    // No config, no state, no saved original, input that is not JSON.
    let o = run(home.path(), &["statusline"], "this is not json");
    assert_eq!((o.status.code(), o.stdout.len(), o.stderr.len()), (Some(0), 0, 0));

    // A corrupt state file does not reach the status line either.
    manage_claude_cli(home.path());
    std::fs::write(Paths::from_home(home.path()).state_file(), "{broken").unwrap();
    let o = run(home.path(), &["statusline"], INPUT);
    assert_eq!((o.status.code(), o.stdout.len(), o.stderr.len()), (Some(0), 0, 0));
}

#[test]
fn install_twice_does_not_wrap_the_wrapper() {
    let home = tempfile::tempdir().unwrap();
    write_settings(home.path(), ORIGINAL);
    run(home.path(), &["statusline", "install"], "");
    let once = std::fs::read_to_string(settings(home.path())).unwrap();
    let o = run(home.path(), &["statusline", "install"], "");
    assert!(text(&o).contains("already installed"), "{}", text(&o));
    assert_eq!(std::fs::read_to_string(settings(home.path())).unwrap(), once);
}

#[test]
fn doctor_reports_the_wrapper() {
    let home = tempfile::tempdir().unwrap();
    assert!(text(&run(home.path(), &["doctor"], "")).contains("statusline: wrapper not installed"));
    run(home.path(), &["statusline", "install"], "");
    assert!(text(&run(home.path(), &["doctor"], "")).contains("statusline: wrapper installed"));
}
