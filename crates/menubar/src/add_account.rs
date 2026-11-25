//! "Add account": open Terminal on the `agent-switch add` command of a provider, either to sign in a new
//! account or to save the login the provider's own app already holds.
//!
//! Adding an account is usually an interactive login, so it cannot run inside the menu bar app. The app writes a
//! small private `.command` script and asks Terminal to open it. The script holds only fixed text and the
//! provider's fixed subcommand. The one value the user types (the Antigravity account id) is read by the
//! script itself and passed quoted, so it never becomes part of the script text.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_switch_core::config::Provider;
use agent_switch_core::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use agent_switch_core::paths::Paths;
use agent_switch_core::{Error, Result};

/// Where `scripts/install.sh` puts the CLI, relative to `$HOME`.
const INSTALLED_CLI: &str = ".local/bin/agent-switch";

/// The start of the file name of a Claude Desktop setup script.
const SETUP_PREFIX: &str = "setup-claude-desktop-";

/// How long a setup script stays before the next setup removes it.
const SETUP_KEEP: std::time::Duration = std::time::Duration::from_secs(600);

/// What the script saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Sign in a new account.
    New,
    /// Save the login that the provider's own app already holds. Antigravity has only this flow: `agy`
    /// signs in on its own, and the CLI saves what it holds.
    Current,
}

/// The file name of the script for `provider`.
fn script_name(provider: Provider, flow: Flow) -> &'static str {
    match (provider, flow) {
        (Provider::Claude, Flow::New) => "add-account-claude.command",
        (Provider::Claude, Flow::Current) => "save-login-claude.command",
        (Provider::Openai, Flow::New) => "add-account-openai.command",
        (Provider::Openai, Flow::Current) => "save-login-openai.command",
        (Provider::Antigravity, _) => "add-account-antigravity.command",
    }
}

/// The shell lines that run once the CLI is found in `$bin`. The last command is the `add`, so `$?` after
/// them is its exit status.
fn provider_lines(provider: Provider, flow: Flow) -> &'static str {
    match (provider, flow) {
        (Provider::Claude, Flow::New) => {
            "echo \"Claude opens next. Type /login and finish signing in. The account is saved right after sign-in. Then type /exit or close this window.\"\n\"$bin\" add claude-cli\n"
        }
        (Provider::Claude, Flow::Current) => {
            "echo \"Saving the login that Claude Code already holds.\"\n\"$bin\" add claude-cli --from-keychain-item \"Claude Code-credentials\"\n"
        }
        (Provider::Openai, Flow::New) => "\"$bin\" add openai\n",
        (Provider::Openai, Flow::Current) => {
            "echo \"Saving the login that Codex already holds.\"\n\"$bin\" add openai --from-file \"$HOME/.codex/auth.json\"\n"
        }
        (Provider::Antigravity, _) => {
            "echo \"Sign in with agy first; this saves the login agy holds.\"\n\
             printf 'Account id (a-z, 0-9, - or _): '\n\
             read -r id\n\
             if [ -z \"$id\" ]; then\n  echo \"No id given.\"\n  false\nelse\n  \"$bin\" add antigravity-cli --id \"$id\"\nfi\n"
        }
    }
}

/// The CLI inside the running app bundle (`Contents/MacOS/agent-switch`), when it is there.
fn bundled_cli() -> Option<PathBuf> {
    let cli = std::env::current_exe().ok()?.parent()?.join("agent-switch");
    cli.is_file().then_some(cli)
}

/// `text` as one single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The text of the script for `provider`. It uses the CLI beside the app executable (`bundled`), else the one
/// that `install.sh` put in `~/.local/bin`, else the one on the login shell's `PATH`. It says whether the
/// `add` saved an account, and waits for Enter at the end so the output can be read.
fn script_with(provider: Provider, flow: Flow, bundled: Option<&Path>) -> String {
    wrap(provider_lines(provider, flow), bundled, "Account saved. The menu updates within a few seconds. You can close this window.", "No account was saved. Read the message above, then close this window.")
}

/// `body` in the script frame: it finds the CLI, runs `body` with `$bin` set, then prints `saved` or `failed`
/// by the exit status of `body`'s last command, and waits for Enter.
fn wrap(body: &str, bundled: Option<&Path>, saved: &str, failed: &str) -> String {
    let bundled = bundled.map(|p| shell_quote(&p.to_string_lossy())).unwrap_or_else(|| "''".to_string());
    format!(
        "#!/bin/zsh -l\n\
         bin={bundled}\n\
         [ -x \"$bin\" ] || bin=\"$HOME/{INSTALLED_CLI}\"\n\
         [ -x \"$bin\" ] || bin=\"$(command -v agent-switch)\"\n\
         rc=1\n\
         if [ -z \"$bin\" ]; then\n  echo \"agent-switch was not found. Install agent-switch with Homebrew: brew install zeemrx0/tap/agent-switch\"\nelse\n{body}rc=$?\nfi\n\
         echo\n\
         if [ \"$rc\" -eq 0 ]; then\n  echo \"{saved}\"\n\
         else\n  echo \"{failed}\"\nfi\n\
         read -r reply\n"
    )
}

/// The text of the script that registers the Claude Desktop profile of `account`. The id is passed as one
/// quoted word, so it never becomes shell syntax.
fn desktop_script_with(account: &str, bundled: Option<&Path>) -> String {
    let body = format!("echo \"Setting up Claude Desktop for this account. Sign in if Claude asks.\"\n\"$bin\" add claude-desktop {}\n", shell_quote(account));
    wrap(&body, bundled, "Claude Desktop is set up. The menu updates within a few seconds. You can close this window.", "Claude Desktop was not set up. Read the message above, then close this window.")
}

/// The script of `provider` and `flow` for this running app.
pub fn script(provider: Provider, flow: Flow) -> String {
    script_with(provider, flow, bundled_cli().as_deref())
}

/// Write a script into the app's private `tmp` folder, executable by the user only.
fn write_private(paths: &Paths, name: &str, text: &str) -> Result<PathBuf> {
    let dir = paths.support_dir().join("tmp");
    ensure_private_dir(&dir)?;
    let path = dir.join(name);
    safe_write(&path, text.as_bytes(), &WriteOpts::secret().with_mode(0o700))?;
    Ok(path)
}

/// Write the script of `provider` and `flow` into the app's private `tmp` folder.
fn write_script(paths: &Paths, provider: Provider, flow: Flow) -> Result<PathBuf> {
    write_private(paths, script_name(provider, flow), &script(provider, flow))
}

/// Open Terminal on the add-account script of `provider`.
pub fn open_terminal(paths: &Paths, provider: Provider, flow: Flow) -> Result<()> {
    open_script(&write_script(paths, provider, flow)?)
}

/// Open Terminal on the script that sets up Claude Desktop for `account`. Each call writes a new file, so a
/// second click cannot replace a script that Terminal has not read yet. The account id reaches the script
/// only as a quoted word, never in a path. Setup scripts older than `SETUP_KEEP` are removed first.
pub fn open_desktop_setup(paths: &Paths, account: &str) -> Result<()> {
    let dir = paths.support_dir().join("tmp");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // A script that is minutes old has been read by Terminal; a newer one may still be waiting.
            let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > SETUP_KEEP);
            if name.starts_with(SETUP_PREFIX) && name.ends_with(".command") && old {
                // Best effort: a script that is already gone is not a failure of this setup.
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("{SETUP_PREFIX}{}-{nanos}.command", std::process::id());
    open_script(&write_private(paths, &name, &desktop_script_with(account, bundled_cli().as_deref()))?)
}

fn open_script(path: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(path)
        .status()
        .map_err(|e| Error::io("run /usr/bin/open", e))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Invalid(format!("Terminal could not open {} ({status})", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn each_script_runs_its_providers_subcommand() {
        assert!(script(Provider::Claude, Flow::New).contains("\"$bin\" add claude-cli\n"));
        let claude = script(Provider::Claude, Flow::New);
        assert!(claude.contains("The account is saved right after sign-in. Then type /exit or close this window."), "{claude}");
        assert!(script(Provider::Openai, Flow::New).contains("\"$bin\" add openai\n"));
        assert!(script(Provider::Antigravity, Flow::New).contains("\"$bin\" add antigravity-cli --id"));
        assert!(!script(Provider::Claude, Flow::New).contains("openai") && !script(Provider::Openai, Flow::New).contains("claude-cli"));
    }

    #[test]
    fn the_cli_beside_the_app_comes_first_and_is_quoted() {
        let s = script_with(Provider::Claude, Flow::New, Some(Path::new("/Apps/Agent Switch.app/it's/agent-switch")));
        assert!(s.contains("bin='/Apps/Agent Switch.app/it'\\''s/agent-switch'\n[ -x \"$bin\" ] || bin=\"$HOME/"), "{s}");
        assert!(script_with(Provider::Claude, Flow::New, None).contains("bin=''\n"));
    }

    #[test]
    fn the_antigravity_id_is_read_by_the_script_and_passed_quoted() {
        let s = script(Provider::Antigravity, Flow::New);
        assert!(s.contains("sign in") || s.contains("Sign in with agy first"), "{s}");
        assert!(s.contains("read -r id"), "{s}");
        assert!(s.contains("--id \"$id\"\n"), "{s}");
        assert!(!s.contains("--id $id"), "{s}");
    }

    #[test]
    fn every_script_finds_the_cli_explains_a_missing_one_and_waits() {
        for p in Provider::ALL {
            let s = script(p, Flow::New);
            assert!(s.starts_with("#!/bin/zsh -l\n"), "{s}");
            assert!(s.contains("bin=\"$HOME/.local/bin/agent-switch\""), "{s}");
            assert!(s.contains("command -v agent-switch"), "{s}");
            assert!(s.contains("brew install zeemrx0/tap/agent-switch") && !s.contains("scripts/install.sh"), "{s}");
            assert!(s.trim_end().ends_with("read -r reply"), "{s}");
        }
    }

    #[test]
    fn saving_the_current_login_imports_it_without_a_new_sign_in() {
        let claude = script(Provider::Claude, Flow::Current);
        assert!(claude.contains("\"$bin\" add claude-cli --from-keychain-item \"Claude Code-credentials\"\n"), "{claude}");
        let openai = script(Provider::Openai, Flow::Current);
        assert!(openai.contains("\"$bin\" add openai --from-file \"$HOME/.codex/auth.json\"\n"), "{openai}");
        assert!(!openai.contains("claude-cli") && !claude.contains("openai"));
        assert_eq!(script(Provider::Antigravity, Flow::Current), script(Provider::Antigravity, Flow::New), "agy has one flow: save the login it holds");
        assert_ne!(script_name(Provider::Claude, Flow::Current), script_name(Provider::Claude, Flow::New));
    }

    #[test]
    fn every_script_reports_whether_an_account_was_saved() {
        for p in Provider::ALL {
            for flow in [Flow::New, Flow::Current] {
                let s = script(p, flow);
                assert!(s.contains("rc=1\n"), "a missing CLI counts as a failure: {s}");
                assert!(s.contains("rc=$?\nfi\n"), "the status is the add's exit status: {s}");
                assert!(s.contains("if [ \"$rc\" -eq 0 ]; then\n  echo \"Account saved. The menu updates within a few seconds."), "{s}");
                assert!(s.contains("else\n  echo \"No account was saved."), "{s}");
                assert!(!s.contains("Done."), "{s}");
            }
        }
        let antigravity = script(Provider::Antigravity, Flow::New);
        assert!(antigravity.contains("echo \"No id given.\"\n  false\n"), "an empty id is a failure: {antigravity}");
    }

    /// Run the script of `provider` and `flow` in zsh with a fake CLI that exits with `code`, and return what it printed.
    fn run_with_fake_cli(provider: Provider, flow: Flow, code: i32, stdin: &str) -> String {
        use std::io::Write;
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("agent-switch");
        std::fs::write(&cli, format!("#!/bin/sh\necho \"fake add $*\"\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = dir.path().join("run.command");
        std::fs::write(&file, script_with(provider, flow, Some(&cli))).unwrap();
        let mut child = Command::new("/bin/zsh")
            .arg(&file)
            .env("HOME", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    #[test]
    fn a_script_says_whether_the_add_saved_an_account() {
        for (provider, flow) in [(Provider::Claude, Flow::New), (Provider::Claude, Flow::Current), (Provider::Openai, Flow::Current)] {
            let ok = run_with_fake_cli(provider, flow, 0, "\n");
            assert!(ok.contains("fake add ") && ok.contains("Account saved. The menu updates within a few seconds."), "{ok}");
            assert!(!ok.contains("No account was saved"), "{ok}");
            let failed = run_with_fake_cli(provider, flow, 1, "\n");
            assert!(failed.contains("fake add ") && failed.contains("No account was saved."), "{failed}");
            assert!(!failed.contains("Account saved."), "{failed}");
        }
        let given = run_with_fake_cli(Provider::Antigravity, Flow::New, 0, "work\n\n");
        assert!(given.contains("antigravity-cli --id work") && given.contains("Account saved."), "{given}");
        let none = run_with_fake_cli(Provider::Antigravity, Flow::New, 0, "\n\n");
        assert!(none.contains("No id given.") && none.contains("No account was saved.") && !none.contains("fake add"), "{none}");
    }

    #[test]
    fn the_desktop_setup_script_passes_the_account_id_quoted() {
        let s = desktop_script_with("claude-zee", Some(Path::new("/Apps/agent-switch")));
        assert!(s.contains("\"$bin\" add claude-desktop 'claude-zee'\nrc=$?\nfi\n"), "{s}");
        assert!(s.contains("Claude Desktop is set up.") && s.contains("Claude Desktop was not set up."), "{s}");
        assert!(s.starts_with("#!/bin/zsh -l\n") && s.trim_end().ends_with("read -r reply"), "{s}");
        let tricky = desktop_script_with("it's", None);
        assert!(tricky.contains("add claude-desktop 'it'\\''s'\n"), "{tricky}");
    }

    #[test]
    fn the_desktop_setup_script_runs_the_add_and_reports_its_status() {
        use std::io::Write;
        use std::process::Stdio;
        for code in [0, 1] {
            let dir = tempfile::tempdir().unwrap();
            let cli = dir.path().join("agent-switch");
            std::fs::write(&cli, format!("#!/bin/sh\necho \"fake add $*\"\nexit {code}\n")).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let file = dir.path().join("run.command");
            std::fs::write(&file, desktop_script_with("it's me", Some(&cli))).unwrap();
            let mut child = Command::new("/bin/zsh").arg(&file).env("HOME", dir.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(b"\n").unwrap();
            let out = child.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains("fake add add claude-desktop it's me"), "{text}");
            assert_eq!(text.contains("Claude Desktop is set up."), code == 0, "{text}");
            assert_eq!(text.contains("Claude Desktop was not set up."), code != 0, "{text}");
        }
    }

    #[test]
    fn the_script_is_written_private_and_executable_for_the_user_only() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(home.path());
        let path = write_script(&paths, Provider::Claude, Flow::New).unwrap();
        assert_eq!(path, paths.support_dir().join("tmp/add-account-claude.command"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), script(Provider::Claude, Flow::New));
        // Writing again replaces the file.
        write_script(&paths, Provider::Claude, Flow::New).unwrap();
    }
}
