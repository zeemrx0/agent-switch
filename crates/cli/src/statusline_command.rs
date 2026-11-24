use std::io::Read;
use std::path::PathBuf;

use agent_switch_core::claude_statusline::{self, Install, Uninstall};
use agent_switch_core::paths::Paths;
use agent_switch_core::state::now_secs;
use agent_switch_core::{Error, Result};

/// What Claude Code runs as its status line command. It must never disturb the status line, so it reports
/// nothing of its own: the exit code is the original command's.
pub fn wrapper() -> i32 {
    let mut input = Vec::new();
    // An unreadable stdin is an empty input: the original command still runs.
    let _ = std::io::stdin().lock().read_to_end(&mut input);
    claude_statusline::run(Paths::from_env().ok().as_ref(), &input, now_secs())
}

/// The path of the running binary, absolute, as the status line command will name it.
fn this_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| Error::io("find the agent-switch binary", e))?;
    if exe.is_absolute() {
        return Ok(exe);
    }
    let cwd = std::env::current_dir().map_err(|e| Error::io("find the current folder", e))?;
    Ok(cwd.join(exe))
}

pub fn install() -> Result<i32> {
    let paths = Paths::from_env()?;
    let binary = this_binary()?;
    let backup = |b: &Option<PathBuf>| b.as_ref().map(|b| format!(" (settings.json backed up to {})", b.display())).unwrap_or_default();
    match claude_statusline::install(&paths, &binary)? {
        Install::Installed { backup: b } => println!("statusline: installed; Claude Code now reports its quota to agent-switch{}", backup(&b)),
        Install::Repointed { backup: b } => println!("statusline: already installed; now runs {}{}", binary.display(), backup(&b)),
        Install::AlreadyInstalled => println!("statusline: already installed"),
    }
    Ok(0)
}

pub fn uninstall() -> Result<i32> {
    let paths = Paths::from_env()?;
    match claude_statusline::uninstall(&paths)? {
        Uninstall::Restored => println!("statusline: removed; your original status line is back in {}", paths.claude_settings_file().display()),
        Uninstall::NotInstalled => println!("statusline: not installed; nothing changed"),
    }
    Ok(0)
}
