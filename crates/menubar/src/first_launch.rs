//! First launch: save the logins that Claude Code and ChatGPT/Codex already hold, once.
//!
//! The app has started for the first time when the config file does not exist. The check creates the file
//! before it reads anything, so it runs once even when nothing is imported (signed out, the Keychain item or
//! `auth.json` is missing, an import fails), and two copies that start together cannot both import. Each
//! provider is independent: one that fails or has no login never stops the other. The accounts are saved the
//! same way `agent-switch add claude-cli --from-keychain-item "Claude Code-credentials"` and
//! `agent-switch add openai --from-file ~/.codex/auth.json` do: no binding changes, and no other app (Claude
//! Desktop, Antigravity) is read.

use agent_switch_core::adapters::claude_code::{ClaudeCode, Imported as ClaudeImported, LIVE_SERVICE};
use agent_switch_core::adapters::codex::{self, Homes, Imported as CodexImported};
use agent_switch_core::config::{mask_email, Config};
use agent_switch_core::surface::Env;

/// What the check did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The config file already existed: this is not the first launch, and nothing was read.
    NotFirst,
    /// The first launch, but no account was saved. The text says why, for the log.
    NothingSaved(String),
    /// The first launch saved at least one account. `notice` is for the user. `skipped` lists, for the log,
    /// the providers that saved nothing.
    Saved { notice: String, skipped: Vec<String> },
}

fn who(email: Option<&str>, id: &str) -> String {
    email.map(mask_email).unwrap_or_else(|| id.to_string())
}

/// "Imported your Claude Code login (a***@example.com) and ChatGPT login (b***@example.com)." Each part is left
/// out when that login was not saved. Each name is the masked email, else the account id.
pub fn saved_notice(claude: Option<&ClaudeImported>, chatgpt: Option<&CodexImported>) -> String {
    let mut parts = Vec::new();
    if let Some(c) = claude {
        parts.push(format!("Claude Code login ({})", who(c.email.as_deref(), &c.id)));
    }
    if let Some(o) = chatgpt {
        parts.push(format!("ChatGPT login ({})", who(o.login.email.as_deref(), &o.id)));
    }
    format!("Imported your {}.", parts.join(" and "))
}

fn import_claude(env: &Env, claude: &ClaudeCode) -> Result<ClaudeImported, String> {
    claude
        .login_source(env, LIVE_SERVICE, None)
        .and_then(|source| claude.import_login(env, &source, None, None, false))
        .map_err(|e| format!("no Claude Code login was saved on the first launch: {e}"))
}

/// Reads `auth.json` from the live Codex home (`~/.codex`), the file the ChatGPT app and the Codex CLI share.
fn import_chatgpt(env: &Env) -> Result<CodexImported, String> {
    let homes = Homes::new(env.paths);
    let copy = codex::read_copy(&homes.primary, "~/.codex")
        .map_err(|e| format!("no ChatGPT login was saved on the first launch: {e}"))?
        .ok_or_else(|| "no ChatGPT login was saved on the first launch: ~/.codex has no auth.json".to_string())?;
    codex::import(env, vec![copy], None, None, false).map_err(|e| format!("no ChatGPT login was saved on the first launch: {e}"))
}

/// Run the first-launch check. It reads the Keychain item and `auth.json` at most once.
pub fn check(env: &Env, claude: &ClaudeCode) -> Outcome {
    match Config::create_if_absent(env.paths) {
        Ok(true) => {}
        Ok(false) => return Outcome::NotFirst,
        Err(e) => return Outcome::NothingSaved(format!("cannot record the first launch: {e}")),
    }
    let claude = import_claude(env, claude);
    let chatgpt = import_chatgpt(env);
    if claude.is_err() && chatgpt.is_err() {
        let why: Vec<String> = [claude.err(), chatgpt.err()].into_iter().flatten().collect();
        return Outcome::NothingSaved(why.join("; "));
    }
    let notice = saved_notice(claude.as_ref().ok(), chatgpt.as_ref().ok());
    let skipped = [claude.err(), chatgpt.err()].into_iter().flatten().collect();
    Outcome::Saved { notice, skipped }
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

    fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

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

        /// Put a ChatGPT login in `~/.codex/auth.json` of the fake home. `email` goes into the id token.
        fn codex_login(&self, email: Option<&str>, account_id: &str) {
            let dir = self.paths.home().join(".codex");
            std::fs::create_dir_all(&dir).unwrap();
            let id_token = email.map(|e| {
                let claims = format!(r#"{{"email":"{e}","https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}","chatgpt_plan_type":"plus"}}}}"#);
                format!("{}.{}.c2ln", b64url(br#"{"alg":"none"}"#), b64url(claims.as_bytes()))
            });
            let id_token = id_token.map(|t| format!(r#""id_token":"{t}","#)).unwrap_or_default();
            let text = format!(
                r#"{{"auth_mode":"chatgpt","tokens":{{{id_token}"access_token":"acc","refresh_token":"rt-1","account_id":"{account_id}"}},"last_refresh":"2026-11-01T00:00:00Z"}}"#
            );
            std::fs::write(dir.join("auth.json"), text).unwrap();
        }

        fn check(&self, email: Option<&'static str>) -> Outcome {
            check(&Env { paths: &self.paths, vault: &self.vault }, &self.claude(email))
        }
    }

    #[test]
    fn the_first_launch_saves_the_login_and_names_the_masked_email() {
        let fx = Fx::new();
        fx.sign_in();
        let Outcome::Saved { notice, .. } = fx.check(Some("jane@example.com")) else { panic!("nothing was saved") };
        assert_eq!(notice, format!("Imported your Claude Code login ({}).", mask_email("jane@example.com")));
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
        let Outcome::Saved { notice, .. } = fx.check(None) else { panic!("nothing was saved") };
        assert_eq!(notice, "Imported your Claude Code login (claude-uuid-f).");
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

    fn saved(outcome: Outcome) -> (String, Vec<String>) {
        match outcome {
            Outcome::Saved { notice, skipped } => (notice, skipped),
            other => panic!("expected a saved account, got {other:?}"),
        }
    }

    #[test]
    fn a_chatgpt_login_alone_is_saved_with_a_masked_notice_and_no_binding() {
        let fx = Fx::new();
        fx.codex_login(Some("bob@example.com"), "acct-1");
        let (notice, skipped) = saved(fx.check(Some("jane@example.com")));
        assert_eq!(notice, format!("Imported your ChatGPT login ({}).", mask_email("bob@example.com")));
        assert!(!notice.contains("bob@"), "{notice}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].starts_with("no Claude Code login was saved on the first launch"), "{skipped:?}");
        let config = Config::load(&fx.paths).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].provider, Provider::Openai);
        assert_eq!(config.surfaces, Default::default(), "the import binds nothing");
    }

    #[test]
    fn both_logins_are_saved_and_one_notice_names_both() {
        let fx = Fx::new();
        fx.sign_in();
        fx.codex_login(Some("bob@example.com"), "acct-1");
        let (notice, skipped) = saved(fx.check(Some("jane@example.com")));
        assert_eq!(
            notice,
            format!("Imported your Claude Code login ({}) and ChatGPT login ({}).", mask_email("jane@example.com"), mask_email("bob@example.com"))
        );
        assert!(skipped.is_empty(), "{skipped:?}");
        let config = Config::load(&fx.paths).unwrap();
        let providers: Vec<_> = config.accounts.iter().map(|a| a.provider).collect();
        assert_eq!(providers, vec![Provider::Claude, Provider::Openai]);
        assert_eq!(config.surfaces, Default::default(), "the import binds nothing");
    }

    #[test]
    fn the_chatgpt_notice_names_the_account_id_when_the_email_is_unknown() {
        let fx = Fx::new();
        fx.codex_login(None, "acct-1");
        let (notice, _) = saved(fx.check(None));
        let id = &Config::load(&fx.paths).unwrap().accounts[0].id;
        assert_eq!(notice, format!("Imported your ChatGPT login ({id})."));
    }

    #[test]
    fn a_missing_auth_file_does_not_stop_the_claude_login() {
        let fx = Fx::new();
        fx.sign_in();
        let (notice, skipped) = saved(fx.check(Some("jane@example.com")));
        assert_eq!(notice, format!("Imported your Claude Code login ({}).", mask_email("jane@example.com")));
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].starts_with("no ChatGPT login was saved on the first launch"), "{skipped:?}");
        assert!(skipped[0].contains("no auth.json"), "{skipped:?}");
    }

    #[test]
    fn a_corrupt_auth_file_is_logged_and_the_claude_login_is_still_saved() {
        let fx = Fx::new();
        fx.sign_in();
        std::fs::create_dir_all(fx.paths.home().join(".codex")).unwrap();
        std::fs::write(fx.paths.home().join(".codex/auth.json"), "not json").unwrap();
        let (notice, skipped) = saved(fx.check(Some("jane@example.com")));
        assert!(notice.starts_with("Imported your Claude Code login"), "{notice}");
        assert!(!notice.contains("ChatGPT"), "{notice}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].starts_with("no ChatGPT login was saved on the first launch"), "{skipped:?}");
        let config = Config::load(&fx.paths).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].provider, Provider::Claude);
    }

    #[test]
    fn nothing_saved_names_both_reasons() {
        let fx = Fx::new();
        let Outcome::NothingSaved(why) = fx.check(None) else { panic!("expected nothing saved") };
        assert!(why.contains("no Claude Code login") && why.contains("no ChatGPT login"), "{why}");
    }

    #[test]
    fn the_chatgpt_login_is_read_once_so_a_later_sign_in_is_not_imported() {
        let fx = Fx::new();
        assert!(matches!(fx.check(None), Outcome::NothingSaved(_)));
        fx.codex_login(Some("bob@example.com"), "acct-1");
        assert_eq!(fx.check(None), Outcome::NotFirst);
        assert!(Config::load(&fx.paths).unwrap().accounts.is_empty());
    }
}
