//! First launch: save the login that Claude Code already holds, once.
//!
//! The app has started for the first time when the config file does not exist. The check creates the file
//! before it reads anything, so it runs once even when nothing is imported (Claude Code is signed out, the
//! Keychain item is missing, the import fails), and two copies that start together cannot both import. It
//! saves the account the same way `agent-switch add claude-cli --from-keychain-item "Claude Code-credentials"`
//! does: no binding changes, and no other app (Claude Desktop, ChatGPT, Codex, Antigravity) is read.

use agent_switch_core::adapters::claude_code::{ClaudeCode, Imported, LIVE_SERVICE};
use agent_switch_core::config::{mask_email, Config};
use agent_switch_core::surface::Env;

/// What the check did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The config file already existed: this is not the first launch, and nothing was read.
    NotFirst,
    /// The first launch, but no account was saved. The text says why, for the log.
    NothingSaved(String),
    /// The first launch saved an account. The text is the notice for the user.
    Saved(String),
}

/// "Saved your Claude Code login (a***@example.com)." with the masked email, else the account id.
pub fn saved_notice(done: &Imported) -> String {
    let who = done.email.as_deref().map(mask_email).unwrap_or_else(|| done.id.clone());
    format!("Saved your Claude Code login ({who}).")
}

/// Run the first-launch check. It reads the Keychain item at most once.
pub fn check(env: &Env, claude: &ClaudeCode) -> Outcome {
    match Config::create_if_absent(env.paths) {
        Ok(true) => {}
        Ok(false) => return Outcome::NotFirst,
        Err(e) => return Outcome::NothingSaved(format!("cannot record the first launch: {e}")),
    }
    let imported = claude
        .login_source(env, LIVE_SERVICE, None)
        .and_then(|source| claude.import_login(env, &source, None, None, false));
    match imported {
        Ok(done) => Outcome::Saved(saved_notice(&done)),
        Err(e) => Outcome::NothingSaved(format!("no Claude Code login was saved on the first launch: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use agent_switch_core::config::{Account, Provider};
    use agent_switch_core::error::Result;
    use agent_switch_core::keychain::{Keychain, MemoryKeychain};
    use agent_switch_core::paths::Paths;
    use agent_switch_core::providers::anthropic::{Profile, ProfileSource};
    use agent_switch_core::token_refresh::RefreshError;
    use agent_switch_core::vault::Vault;

    const USER: &str = "tester";

    /// A profile endpoint that knows one account.
    struct OneProfile(Option<&'static str>);

    impl ProfileSource for OneProfile {
        fn profile(&self, _access_token: &str) -> Result<Profile> {
            Ok(Profile {
                account_uuid: "uuid-first".into(),
                email: self.0.map(str::to_string),
                display_name: None,
                full_name: None,
                organization_uuid: None,
                organization_name: None,
                organization_type: Some("claude_max".into()),
                rate_limit_tier: None,
            })
        }

        fn refresh_login(&self, _blob: &str, _now: u64) -> std::result::Result<String, RefreshError> {
            Err(RefreshError::Transient { detail: "offline".into(), may_have_consumed: false })
        }
    }

    struct Fx {
        _dir: tempfile::TempDir,
        paths: Paths,
        vault: Vault,
        live: Arc<MemoryKeychain>,
    }

    impl Fx {
        fn new() -> Fx {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::from_home(dir.path());
            Fx { _dir: dir, paths, vault: Vault::new(Arc::new(MemoryKeychain::new())), live: Arc::new(MemoryKeychain::new()) }
        }

        fn sign_in(&self) {
            let login = r#"{"claudeAiOauth":{"accessToken":"acc-1","refreshToken":"ref-1","expiresAt":1,"subscriptionType":"max"}}"#;
            self.live.set(LIVE_SERVICE, USER, login).unwrap();
        }

        fn claude(&self, email: Option<&'static str>) -> ClaudeCode {
            ClaudeCode::new(self.live.clone(), Arc::new(OneProfile(email)), Arc::new(|k| (k == "USER").then(|| USER.to_string())))
        }

        fn check(&self, email: Option<&'static str>) -> Outcome {
            check(&Env { paths: &self.paths, vault: &self.vault }, &self.claude(email))
        }
    }

    #[test]
    fn the_first_launch_saves_the_login_and_names_the_masked_email() {
        let fx = Fx::new();
        fx.sign_in();
        let Outcome::Saved(notice) = fx.check(Some("jane@example.com")) else { panic!("nothing was saved") };
        assert_eq!(notice, format!("Saved your Claude Code login ({}).", mask_email("jane@example.com")));
        assert!(!notice.contains("jane@"), "{notice}");
        let config = Config::load(&fx.paths).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].provider, Provider::Claude);
        assert_eq!(config.accounts[0].id, "claude-jane");
        assert_eq!(config.surfaces, Default::default(), "the import binds nothing");
    }

    #[test]
    fn the_notice_names_the_account_id_when_the_email_is_unknown() {
        let fx = Fx::new();
        fx.sign_in();
        let Outcome::Saved(notice) = fx.check(None) else { panic!("nothing was saved") };
        assert_eq!(notice, "Saved your Claude Code login (claude-uuid-f).");
    }

    #[test]
    fn an_existing_config_is_never_touched() {
        let fx = Fx::new();
        fx.sign_in();
        let mut config = Config::default();
        config.add_account(Account { id: "mine".into(), provider: Provider::Claude, label: "Mine".into(), email: None, uuid: None }).unwrap();
        config.save(&fx.paths).unwrap();
        assert_eq!(fx.check(Some("jane@example.com")), Outcome::NotFirst);
        assert_eq!(Config::load(&fx.paths).unwrap(), config);
    }

    #[test]
    fn the_config_exists_afterwards_even_when_there_is_no_login_so_the_check_runs_once() {
        let fx = Fx::new();
        assert!(!fx.paths.config_file().exists());
        assert!(matches!(fx.check(Some("jane@example.com")), Outcome::NothingSaved(_)));
        assert!(fx.paths.config_file().exists());
        assert!(Config::load(&fx.paths).unwrap().accounts.is_empty());

        // Signing in later does not make the next start import it.
        fx.sign_in();
        assert_eq!(fx.check(Some("jane@example.com")), Outcome::NotFirst);
        assert!(Config::load(&fx.paths).unwrap().accounts.is_empty());
    }

    #[test]
    fn a_login_that_cannot_be_saved_is_logged_not_raised_and_is_not_retried() {
        let fx = Fx::new();
        fx.live.set(LIVE_SERVICE, USER, "not json").unwrap();
        let Outcome::NothingSaved(why) = fx.check(Some("jane@example.com")) else { panic!("expected nothing saved") };
        assert!(why.starts_with("no Claude Code login was saved on the first launch"), "{why}");
        assert_eq!(fx.check(Some("jane@example.com")), Outcome::NotFirst);
    }

    #[test]
    fn an_unwritable_config_folder_saves_nothing_and_reports_why() {
        let fx = Fx::new();
        fx.sign_in();
        // A file where the config folder must be.
        std::fs::create_dir_all(fx.paths.home().join(".config")).unwrap();
        std::fs::write(fx.paths.home().join(".config/agent-switch"), "").unwrap();
        let Outcome::NothingSaved(why) = fx.check(Some("jane@example.com")) else { panic!("expected nothing saved") };
        assert!(why.starts_with("cannot record the first launch"), "{why}");
    }
}
