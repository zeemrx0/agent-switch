use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::config::Binding;
use crate::keychain::MemoryKeychain;
use crate::token_refresh::RefreshError;
use crate::state::State;
use crate::switch;
use crate::vault::Vault;

const USER: &str = "tester";

// ---------------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------------

/// The live Keychain: a memory store whose next `set` can be made to fail.
#[derive(Default)]
struct Flaky {
    inner: MemoryKeychain,
    fail_next_set: AtomicBool,
    fail_every_set: AtomicBool,
}

impl Keychain for Flaky {
    fn get(&self, s: &str, a: &str) -> Result<Option<String>> {
        self.inner.get(s, a)
    }
    fn set(&self, s: &str, a: &str, v: &str) -> Result<()> {
        if self.fail_next_set.swap(false, Ordering::SeqCst) || self.fail_every_set.load(Ordering::SeqCst) {
            return Err(Error::Keychain("injected failure".into()));
        }
        self.inner.set(s, a, v)
    }
    fn delete(&self, s: &str, a: &str) -> Result<bool> {
        self.inner.delete(s, a)
    }
    fn name(&self) -> &'static str {
        "flaky"
    }
}

/// Maps an access token to the account uuid the profile endpoint would report.
#[derive(Default)]
struct FakeProfiles {
    by_token: Mutex<HashMap<String, String>>,
    calls: AtomicUsize,
    /// What the next refresh answers: `Ok` rotates to `acc-new` / `ref-new`; unset is a transient failure.
    refresh_reply: Mutex<Option<std::result::Result<(), RefreshError>>>,
    refresh_calls: AtomicUsize,
    /// Runs inside a refresh request, to play what Claude Code does while the request is in flight.
    during_refresh: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl FakeProfiles {
    fn know(&self, token: &str, uuid: &str) {
        self.by_token.lock().unwrap().insert(token.into(), uuid.into());
    }
}

impl ProfileSource for FakeProfiles {
    fn refresh_login(&self, blob: &str, now: u64) -> std::result::Result<String, RefreshError> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.during_refresh.lock().unwrap().as_ref() {
            hook();
        }
        match self.refresh_reply.lock().unwrap().clone() {
            Some(Ok(())) => {
                let resp = json!({"access_token": "acc-new", "expires_in": 3600, "refresh_token": "ref-new", "scope": "user:inference user:profile"});
                Ok(crate::providers::anthropic::apply_refresh(blob, &resp, now * 1000).unwrap())
            }
            Some(Err(e)) => Err(e),
            None => Err(RefreshError::Transient { detail: "the token endpoint answered HTTP 503".into(), may_have_consumed: false }),
        }
    }

    fn profile(&self, access_token: &str) -> Result<Profile> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.by_token.lock().unwrap().get(access_token) {
            Some(uuid) => Ok(Profile {
                account_uuid: uuid.clone(),
                email: Some(format!("{uuid}@example.com")),
                display_name: None,
                full_name: None,
                organization_uuid: Some("org-1".into()),
                organization_name: None,
                organization_type: Some("claude_max".into()),
                rate_limit_tier: None,
            }),
            None => Err(Error::Invalid("profile request was refused (HTTP 401)".into())),
        }
    }
}

fn login(access: &str, refresh: &str) -> Obj {
    let mut o = Obj::new();
    o.insert(
        "claudeAiOauth".into(),
        json!({"accessToken": access, "refreshToken": refresh, "expiresAt": 1, "subscriptionType": "max"}),
    );
    o
}

fn with(mut o: Obj, k: &str, v: Value) -> Obj {
    o.insert(k.into(), v);
    o
}

fn oauth_account(uuid: &str) -> Value {
    json!({"accountUuid": uuid, "emailAddress": format!("{uuid}@example.com"), "billingType": "stripe"})
}

fn saved(login: Obj, uuid: &str) -> Slot {
    ClaudeSlot::new(account_bound(&login), oauth_account(uuid)).to_slot(1).unwrap()
}

struct Fx {
    _dir: tempfile::TempDir,
    paths: Paths,
    vault: Vault,
    live: Arc<Flaky>,
    profiles: Arc<FakeProfiles>,
}

const MCP: &str = r#"{"srv":{"accessToken":"mcp-live","n":1}}"#;

impl Fx {
    /// Live login is `personal` (access `acc-P1`) with MCP logins; `team` is saved; `~/.claude.json` names personal.
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(dir.path());
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let vault = Vault::new(Arc::new(MemoryKeychain::new()));
        let live = Arc::new(Flaky::default());
        let profiles = Arc::new(FakeProfiles::default());

        let mut config = Config::default();
        for (id, uuid) in [("personal", "u-personal"), ("team", "u-team")] {
            config
                .add_account(Account { id: id.into(), provider: Provider::Claude, label: id.into(), email: None, uuid: Some(uuid.into()) })
                .unwrap();
        }
        config.save(&paths).unwrap();

        let personal = with(with(login("acc-P1", "ref-P1"), "trustedDeviceToken", json!("tdt-P")), "unknownP", json!("p-only"));
        let team = with(with(with(login("acc-T1", "ref-T1"), "trustedDeviceToken", json!("tdt-T")), "unknownT", json!("keep")), "mcpOAuth", json!({"stale": 1}));
        vault.put("personal", STORE, &saved(personal.clone(), "u-personal")).unwrap();
        vault.put("team", STORE, &saved(team, "u-team")).unwrap();

        let mut live_obj = with(personal, "mcpOAuth", serde_json::from_str(MCP).unwrap());
        live_obj.insert("unknownLiveOnly".into(), json!(1));
        live.set(LIVE_SERVICE, USER, &compact_ascii(&live_obj).unwrap()).unwrap();

        let claude_json = serde_json::to_string_pretty(&json!({
            "numStartups": 5,
            "oauthAccount": oauth_account("u-personal"),
            "projects": {"/x": {"allowedTools": []}},
            "mcpServers": {"a": {"command": "x"}},
        }))
        .unwrap();
        std::fs::write(dir.path().join(".claude.json"), claude_json).unwrap();

        profiles.know("acc-P1", "u-personal");
        Fx { _dir: dir, paths, vault, live, profiles }
    }

    fn adapter(&self) -> ClaudeCode {
        self.adapter_with_env(&[])
    }

    fn adapter_with_env(&self, vars: &[(&str, &str)]) -> ClaudeCode {
        let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        map.insert("USER".into(), USER.into());
        ClaudeCode::new(self.live.clone(), self.profiles.clone(), Arc::new(move |k| map.get(k).cloned()))
    }

    fn env(&self) -> Env<'_> {
        Env { paths: &self.paths, vault: &self.vault }
    }

    fn config(&self) -> Config {
        Config::load(&self.paths).unwrap()
    }

    fn switch(&self, adapter: &ClaudeCode, to: &str) -> Result<()> {
        switch::activate(&self.env(), &self.config(), adapter, to)
    }

    fn live_obj(&self) -> Obj {
        parse_login(&self.live.get(LIVE_SERVICE, USER).unwrap().unwrap()).unwrap()
    }

    fn live_text(&self) -> String {
        self.live.get(LIVE_SERVICE, USER).unwrap().unwrap()
    }

    fn claude_json_path(&self) -> PathBuf {
        self.paths.home().join(".claude.json")
    }

    fn claude_json(&self) -> String {
        std::fs::read_to_string(self.claude_json_path()).unwrap()
    }

    fn slot(&self, id: &str) -> ClaudeSlot {
        ClaudeSlot::parse(&self.vault.get(id, STORE).unwrap().unwrap().blob).unwrap()
    }

    fn set_live(&self, obj: &Obj) {
        self.live.set(LIVE_SERVICE, USER, &compact_ascii(obj).unwrap()).unwrap();
    }
}

fn access(o: &Obj) -> String {
    token(o, "accessToken").unwrap().to_string()
}

// ---------------------------------------------------------------------------------------------
// Item names and environment
// ---------------------------------------------------------------------------------------------

#[test]
fn hashed_item_name_matches_a_known_pair() {
    // Same scheme as a pair seen live with `CLAUDE_CONFIG_DIR=~/.agent-switch/tmp/claude-b` then `/login` (2026-11-06).
    assert_eq!(keychain_service_name("/Users/me/.agent-switch/tmp/claude-b"), "Claude Code-credentials-d262906e");
    // A trailing slash is a different raw string, hence a different item: the raw value is hashed.
    assert_ne!(keychain_service_name("/Users/me/.agent-switch/tmp/claude-b/"), "Claude Code-credentials-d262906e");
    // NFC normalization: composed and decomposed spellings hash the same.
    assert_eq!(keychain_service_name("/tmp/caf\u{e9}"), keychain_service_name("/tmp/cafe\u{301}"));
}

#[test]
fn config_dir_variables_select_the_item() {
    let fx = Fx::new();
    let services = |vars: &[(&str, &str)]| Setup::resolve(&fx.paths, &fx.adapter_with_env(vars).getenv).services;
    assert_eq!(services(&[]), vec![LIVE_SERVICE]);
    assert_eq!(services(&[("CLAUDE_CONFIG_DIR", "/work/claude-b")]), vec![keychain_service_name("/work/claude-b")]);
    // Defined-but-empty secure-storage dir selects the default item; a value wins over CLAUDE_CONFIG_DIR.
    assert_eq!(services(&[("CLAUDE_SECURESTORAGE_CONFIG_DIR", ""), ("CLAUDE_CONFIG_DIR", "/work/x")]), vec![LIVE_SERVICE]);
    assert_eq!(
        services(&[("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/s"), ("CLAUDE_CONFIG_DIR", "/work/x")]),
        vec![keychain_service_name("/s")]
    );
    // A config dir naming the default profile may still only have the unsuffixed item.
    let default_dir = fx.paths.home().join(".claude");
    let both = services(&[("CLAUDE_CONFIG_DIR", default_dir.to_str().unwrap())]);
    assert_eq!(both, vec![keychain_service_name(default_dir.to_str().unwrap()), LIVE_SERVICE.to_string()]);

    let setup = Setup::resolve(&fx.paths, &fx.adapter_with_env(&[("CLAUDE_CONFIG_DIR", "/work/claude-b")]).getenv);
    assert_eq!(setup.global_config, PathBuf::from("/work/claude-b/.claude.json"));
    assert_eq!(setup.refresh_lock(), PathBuf::from("/work/claude-b/.oauth_refresh.lock"));
    assert_eq!(setup.legacy_lock(), PathBuf::from("/work/claude-b.lock"));
}

#[test]
fn an_override_variable_refuses_every_entry_point() {
    for var in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"] {
        let fx = Fx::new();
        let a = fx.adapter_with_env(&[(var, "x")]);
        let err = fx.switch(&a, "team").unwrap_err().to_string();
        assert!(err.contains(var) && err.contains("instead of the saved OAuth login"), "{err}");
        assert!(a.add_interactive(&fx.env(), None, None).is_err());
        let src = LoginSource { service: LIVE_SERVICE.into(), config_json: None };
        assert!(a.import_login(&fx.env(), &src, Some("x"), None, true).is_err());
        assert!(fx.live_text().contains("ref-P1"), "live login must be untouched");
        assert!(a.doctor_checks(&fx.env()).iter().any(|c| c.level == Level::Fail && c.detail.contains(var)));
    }
    // Empty values do not count as set.
    let fx = Fx::new();
    fx.switch(&fx.adapter_with_env(&[("ANTHROPIC_API_KEY", "")]), "team").unwrap();
}

// ---------------------------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------------------------

#[test]
fn merge_keeps_live_shared_keys_and_takes_everything_else_from_the_target() {
    let live = with(with(with(login("acc-L", "ref-L"), "mcpOAuth", json!({"srv": {"t": 1}})), "trustedDeviceToken", json!("tdt-live")), "unknownLive", json!(true));
    let target = with(
        with(with(with(login("acc-T", "ref-T"), "mcpOAuth", json!({"old": 1})), "pluginSecrets", json!({"gone": 1})), "trustedDeviceToken", json!("tdt-T")),
        "unknownTarget",
        json!("kept"),
    );
    let merged = merge_login(Some(&live), &target);
    assert_eq!(merged["mcpOAuth"], json!({"srv": {"t": 1}}), "shared key comes from the live item");
    assert!(!merged.contains_key("pluginSecrets"), "a shared key the machine no longer has is not resurrected");
    assert_eq!(merged["claudeAiOauth"], target["claudeAiOauth"]);
    assert_eq!(merged["trustedDeviceToken"], json!("tdt-T"), "trustedDeviceToken follows the account");
    assert_eq!(merged["unknownTarget"], json!("kept"), "unknown keys are account-bound");
    assert!(!merged.contains_key("unknownLive"), "a live unknown key is not carried to another account");

    let alone = merge_login(None, &target);
    assert!(!alone.contains_key("mcpOAuth") && alone.contains_key("claudeAiOauth"));
    let bound = account_bound(&live);
    assert!(!bound.contains_key("mcpOAuth") && bound.contains_key("unknownLive") && bound.contains_key("trustedDeviceToken"));
}

#[test]
fn live_text_in_hex_form_is_accepted() {
    let hex: String = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#.bytes().map(|b| format!("{b:02x}")).collect();
    assert!(parse_login(&hex).is_ok());
    assert!(parse_login("not json").is_err());
    assert!(parse_login("[]").is_err());
}

// ---------------------------------------------------------------------------------------------
// Switch, save-back, switching back
// ---------------------------------------------------------------------------------------------

#[test]
fn switch_replaces_the_login_and_identity_and_keeps_everything_else() {
    let fx = Fx::new();
    let before_json = fx.claude_json();
    let a = fx.adapter();
    fx.switch(&a, "personal").unwrap(); // live already is personal: a no-op switch that still journals once

    // Claude Code refreshed the personal access token (same refresh token) before the switch.
    let mut refreshed = fx.live_obj();
    refreshed.insert("claudeAiOauth".into(), json!({"accessToken": "acc-P2", "refreshToken": "ref-P1", "expiresAt": 2}));
    fx.set_live(&refreshed);

    fx.switch(&a, "team").unwrap();

    let live = fx.live_obj();
    assert_eq!(access(&live), "acc-T1");
    assert_eq!(live["trustedDeviceToken"], json!("tdt-T"));
    assert_eq!(live["unknownT"], json!("keep"));
    assert_eq!(live["mcpOAuth"], serde_json::from_str::<Value>(MCP).unwrap(), "MCP logins stay byte-for-byte as the live item had them");
    assert!(!live.contains_key("unknownP") && !live.contains_key("unknownLiveOnly"));

    // ~/.claude.json: only oauthAccount changed; key order and every other value are as before.
    let after: Value = serde_json::from_str(&fx.claude_json()).unwrap();
    let mut expected: Value = serde_json::from_str(&before_json).unwrap();
    expected["oauthAccount"] = oauth_account("u-team");
    assert_eq!(after, expected);
    let keys: Vec<_> = after.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["numStartups", "oauthAccount", "projects", "mcpServers"]);
    assert!(!fx.claude_json().ends_with('\n'), "trailing newline state is preserved");

    // The refreshed personal token was saved back before leaving.
    assert_eq!(access(&fx.slot("personal").login), "acc-P2");
    assert_eq!(fx.slot("personal").login["trustedDeviceToken"], json!("tdt-P"));
    let st = State::load(&fx.paths).unwrap();
    assert_eq!(st.active(SurfaceKind::ClaudeCli), Some("team"));
}

#[test]
fn switching_back_restores_the_account_that_was_refreshed_in_between() {
    let fx = Fx::new();
    let a = fx.adapter();
    fx.switch(&a, "personal").unwrap();
    // personal refreshes in the live item and rotates its refresh token; the profile endpoint knows the new token.
    let mut rotated = fx.live_obj();
    rotated.insert("claudeAiOauth".into(), json!({"accessToken": "acc-P2", "refreshToken": "ref-P2", "expiresAt": 2}));
    fx.set_live(&rotated);
    fx.profiles.know("acc-P2", "u-personal");

    fx.switch(&a, "team").unwrap();
    assert_eq!(token(&fx.slot("personal").login, "refreshToken").unwrap(), "ref-P2", "the rotated login was saved back through the identity check");

    // The live item is team's. A later refresh of team's login is saved back on the way out.
    let mut team_live = fx.live_obj();
    team_live.insert("claudeAiOauth".into(), json!({"accessToken": "acc-T2", "refreshToken": "ref-T1", "expiresAt": 3}));
    fx.set_live(&team_live);

    fx.switch(&a, "personal").unwrap();
    let live = fx.live_obj();
    assert_eq!(token(&live, "refreshToken").unwrap(), "ref-P2");
    assert_eq!(live["trustedDeviceToken"], json!("tdt-P"));
    assert_eq!(live["mcpOAuth"], serde_json::from_str::<Value>(MCP).unwrap());
    assert_eq!(access(&fx.slot("team").login), "acc-T2");
    let json: Value = serde_json::from_str(&fx.claude_json()).unwrap();
    assert_eq!(json["oauthAccount"]["accountUuid"], "u-personal");
}

#[test]
fn a_matching_fingerprint_needs_no_network_call() {
    let fx = Fx::new();
    fx.switch(&fx.adapter(), "team").unwrap();
    assert_eq!(fx.profiles.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn logged_out_live_item_is_created() {
    let fx = Fx::new();
    fx.live.delete(LIVE_SERVICE, USER).unwrap();
    fx.switch(&fx.adapter(), "team").unwrap();
    let live = fx.live_obj();
    assert_eq!(access(&live), "acc-T1");
    assert!(!live.contains_key("mcpOAuth"));
}

#[test]
fn an_existing_credentials_file_is_rewritten_so_a_running_session_notices() {
    let fx = Fx::new();
    let file = fx.paths.home().join(".claude/.credentials.json");
    fx.switch(&fx.adapter(), "team").unwrap();
    assert!(!file.exists(), "the file is never created");

    std::fs::write(&file, "old").unwrap();
    fx.switch(&fx.adapter(), "personal").unwrap();
    let written = parse_login(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(access(&written), "acc-P1");
    assert_eq!(written, fx.live_obj());
}

// ---------------------------------------------------------------------------------------------
// Ownership and quarantine
// ---------------------------------------------------------------------------------------------

#[test]
fn classification_prefers_the_fingerprint_then_the_profile_identity() {
    let slot = |id: &str, refresh: &str, uuid: &str| SavedLogin {
        id: id.into(),
        fp: lineage_fingerprint(refresh),
        slot: Some(ClaudeSlot::new(login("a", refresh), oauth_account(uuid))),
    };
    let saved = [slot("a", "ra", "ua"), slot("b", "rb", "ub")];
    assert_eq!(classify(&lineage_fingerprint("rb"), &saved, Some("ua")), Owner::Account("b".into()));
    assert_eq!(classify(&lineage_fingerprint("rotated"), &saved, Some("ua")), Owner::Account("a".into()));
    let fp = lineage_fingerprint("rotated");
    assert_eq!(classify(&fp, &saved, Some("stranger")), Owner::Unknown { fp: fp.clone(), resolved: true });
    assert_eq!(classify(&fp, &saved, None), Owner::Unknown { fp, resolved: false });
}

#[test]
fn a_login_nobody_saved_is_quarantined_and_the_switch_stops_untouched() {
    let fx = Fx::new();
    let stranger = with(login("acc-X", "ref-X"), "mcpOAuth", json!({"x": 1}));
    fx.set_live(&stranger);
    fx.profiles.know("acc-X", "u-stranger");
    let (live_before, json_before) = (fx.live_text(), fx.claude_json());
    let (p_before, t_before) = (fx.vault.get("personal", STORE).unwrap(), fx.vault.get("team", STORE).unwrap());

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();
    let stash = format!("quarantine-{}", lineage_fingerprint("ref-X"));
    assert!(err.contains("does not belong to any saved account") && err.contains(&stash) && err.contains("--from-keychain-item"), "{err}");
    assert!(!err.contains("Nothing was changed"), "a recovery copy was written, so the message must not claim that: {err}");
    assert!(err.contains(&format!("remove {stash}")), "{err}");

    assert_eq!(fx.live_text(), live_before);
    assert_eq!(fx.claude_json(), json_before);
    assert_eq!(fx.vault.get("personal", STORE).unwrap(), p_before, "no account slot is written");
    assert_eq!(fx.vault.get("team", STORE).unwrap(), t_before);
    let state = State::load(&fx.paths).unwrap();
    assert_eq!(State { quarantine: Default::default(), ..state.clone() }, State::default(), "no switch is recorded");
    assert!(state.quarantine.contains_key(&stash), "the recovery copy is tracked so doctor can list it and remove can delete it");
    let stashed = ClaudeSlot::parse(&fx.vault.get(&stash, STORE).unwrap().unwrap().blob).unwrap();
    assert_eq!(access(&stashed.login), "acc-X");
    assert!(!stashed.login.contains_key("mcpOAuth"), "only the account-bound part is stashed");
}

#[test]
fn quarantine_copies_are_bounded_and_one_per_login() {
    let fx = Fx::new();
    let adapter = fx.adapter();
    let mut stashes = Vec::new();
    for n in 0..5 {
        let stranger = login(&format!("acc-X{n}"), &format!("ref-X{n}"));
        fx.set_live(&stranger);
        fx.profiles.know(&format!("acc-X{n}"), &format!("u-stranger-{n}"));
        assert!(fx.switch(&adapter, "team").is_err());
        stashes.push(format!("quarantine-{}", lineage_fingerprint(&format!("ref-X{n}"))));
    }
    // The same login again overwrites its own copy.
    assert!(fx.switch(&adapter, "team").is_err());
    let kept = State::load(&fx.paths).unwrap().quarantine;
    assert_eq!(kept.len(), crate::state::QUARANTINE_LIMIT, "{kept:?}");
    let in_vault = stashes.iter().filter(|id| fx.vault.get(id, STORE).unwrap().is_some()).count();
    assert_eq!(in_vault, crate::state::QUARANTINE_LIMIT, "vault copies match the tracked ones");
    assert!(kept.keys().all(|id| fx.vault.get(id, STORE).unwrap().is_some()));
}

#[test]
fn the_owner_slot_is_read_after_its_account_lock_is_taken_and_outside_claude_codes_locks() {
    let fx = Fx::new();
    let adapter = fx.adapter();
    // `personal` owns the live login but state does not say so, so `activate` does not hold its lock.
    let held = AccountLock::acquire(&fx.paths, "personal", Duration::from_secs(1)).unwrap();
    let refresh_lock = fx.paths.home().join(".claude/.oauth_refresh.lock");
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| fx.switch(&adapter, "team"));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!refresh_lock.exists(), "waiting for the account lock must not happen while Claude Code's lock is held");
        // Another process updates personal's slot while the switch waits.
        let mut slot = fx.slot("personal");
        slot.oauth_account["billingType"] = json!("changed-elsewhere");
        fx.vault.put("personal", STORE, &slot.to_slot(2).unwrap()).unwrap();
        drop(held);
        worker.join().unwrap()
    });
    result.unwrap();
    assert_eq!(fx.slot("personal").oauth_account["billingType"], json!("changed-elsewhere"), "the save-back kept the newer slot data");
}

#[test]
fn a_live_login_that_changed_owner_while_switching_stops_and_can_be_retried() {
    let fx = Fx::new();
    let adapter = fx.adapter();
    let payload = StagePayload { slot: fx.vault.get("team", STORE).unwrap().unwrap().blob, probe: None };
    let setup = adapter.setup(&fx.env());
    // The caller locked only `team`, but `personal` owns the live login.
    let err = adapter.transact(&fx.env(), &setup, &payload, "team", &["team".to_string()]).unwrap_err().to_string();
    assert!(err.contains("changed to `personal`") && err.contains("run the command again"), "{err}");
    assert!(fx.live_text().contains("ref-P1"), "nothing was written");
}

#[test]
fn an_unverifiable_live_login_stops_with_a_retry_hint() {
    let fx = Fx::new();
    let mut rotated = fx.live_obj();
    rotated.insert("claudeAiOauth".into(), json!({"accessToken": "acc-unknown", "refreshToken": "ref-new", "expiresAt": 2}));
    fx.set_live(&rotated); // the profile endpoint refuses this token
    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();
    assert!(err.contains("could not be matched to a saved account") && err.contains("401") && err.contains("retry"), "{err}");
    assert!(fx.vault.get(&format!("quarantine-{}", lineage_fingerprint("ref-new")), STORE).unwrap().is_some());
    assert_eq!(token(&fx.slot("personal").login, "refreshToken").unwrap(), "ref-P1");
}

// ---------------------------------------------------------------------------------------------
// Refreshing an expired live login
// ---------------------------------------------------------------------------------------------

/// The live login after Claude Code rotated its refresh token and the access token expired: no saved
/// fingerprint matches and the profile endpoint refuses the old access token.
fn expire_live(fx: &Fx, expires_at: u64) {
    let mut live = fx.live_obj();
    live.insert("claudeAiOauth".into(), json!({"accessToken": "acc-P2", "refreshToken": "ref-P2", "expiresAt": expires_at, "subscriptionType": "max"}));
    fx.set_live(&live);
}

fn stage_team(fx: &Fx, adapter: &ClaudeCode) -> Result<Staged> {
    let config = fx.config();
    adapter.stage(&fx.env(), config.account("team").unwrap())
}

#[test]
fn an_expired_live_login_is_refreshed_in_place_and_keeps_its_other_keys() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    let creds_file = fx.paths.home().join(".claude").join(".credentials.json");
    std::fs::write(&creds_file, "{}").unwrap();
    fx.profiles.know("acc-new", "u-personal");
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Ok(()));
    let before = fx.live_obj();

    stage_team(&fx, &fx.adapter()).unwrap();

    let live = fx.live_obj();
    let oauth = oauth_of(&live).unwrap();
    assert_eq!((oauth["accessToken"].as_str(), oauth["refreshToken"].as_str()), (Some("acc-new"), Some("ref-new")));
    assert_eq!(oauth["scopes"], json!(["user:inference", "user:profile"]));
    assert!(oauth["expiresAt"].as_u64().unwrap() > now_secs() * 1000);
    assert_eq!(oauth["subscriptionType"], "max");
    for key in ["mcpOAuth", "trustedDeviceToken", "unknownP", "unknownLiveOnly"] {
        assert_eq!(live.get(key), before.get(key), "{key} must stay as it was");
    }
    assert_eq!(std::fs::read_to_string(&creds_file).unwrap(), fx.live_text(), "a running Claude Code notices the new file");
    assert_eq!(fx.profiles.refresh_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_switch_proceeds_once_the_expired_live_login_is_refreshed() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    fx.profiles.know("acc-new", "u-personal");
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Ok(()));

    fx.switch(&fx.adapter(), "team").unwrap();

    assert_eq!(access(&fx.live_obj()), "acc-T1");
    assert_eq!(fx.live_obj().get("mcpOAuth"), Some(&serde_json::from_str::<Value>(MCP).unwrap()));
    assert_eq!(token(&fx.slot("personal").login, "refreshToken").unwrap(), "ref-new", "the outgoing login is saved back");
    assert!(State::load(&fx.paths).unwrap().quarantine.is_empty());
}

#[test]
fn a_live_login_that_is_still_valid_is_never_refreshed() {
    let fx = Fx::new();
    expire_live(&fx, (now_secs() + 3600) * 1000);
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Ok(()));
    let live_before = fx.live_text();

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();

    assert!(err.contains("could not be matched to a saved account") && err.contains("401"), "{err}");
    assert_eq!(fx.profiles.refresh_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fx.live_text(), live_before);
}

#[test]
fn a_live_login_that_claude_code_replaced_during_the_refresh_is_not_overwritten() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    fx.profiles.know("acc-P3", "u-personal");
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Ok(()));
    let (live, user) = (fx.live.clone(), USER);
    let mut newer = fx.live_obj();
    newer.insert("claudeAiOauth".into(), json!({"accessToken": "acc-P3", "refreshToken": "ref-P3", "expiresAt": 4_000_000_000_000u64, "subscriptionType": "max"}));
    let newer_text = compact_ascii(&newer).unwrap();
    *fx.profiles.during_refresh.lock().unwrap() = Some(Box::new(move || live.set(LIVE_SERVICE, user, &newer_text).unwrap()));

    fx.switch(&fx.adapter(), "team").unwrap();

    assert_eq!(fx.profiles.refresh_calls.load(Ordering::SeqCst), 1);
    assert_eq!(token(&fx.slot("personal").login, "refreshToken").unwrap(), "ref-P3", "the newer login, not ours, was saved back");
    assert!(!fx.vault.get("personal", STORE).unwrap().unwrap().blob.contains("ref-new"));
}

#[test]
fn a_rejected_live_refresh_token_asks_for_a_new_sign_in_and_writes_nothing() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Err(RefreshError::Dead("the refresh token was rejected (invalid_grant)".into())));
    let (live_before, personal_before) = (fx.live_text(), fx.vault.get("personal", STORE).unwrap());

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();

    assert!(err.contains("Sign in to Claude Code again") && err.contains("invalid_grant"), "{err}");
    assert_eq!(fx.live_text(), live_before);
    assert_eq!(fx.vault.get("personal", STORE).unwrap(), personal_before);
}

#[test]
fn a_rotated_login_that_cannot_be_written_back_is_kept_as_a_recovery_copy() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Ok(()));
    fx.live.fail_every_set.store(true, Ordering::SeqCst);

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();

    let stash = format!("quarantine-{}", lineage_fingerprint("ref-new"));
    assert!(err.contains("Sign in to Claude Code again") && err.contains(&stash) && err.contains("injected failure"), "{err}");
    let kept = ClaudeSlot::parse(&fx.vault.get(&stash, STORE).unwrap().unwrap().blob).unwrap();
    assert_eq!((access(&kept.login).as_str(), token(&kept.login, "refreshToken").unwrap()), ("acc-new", "ref-new"));
    assert!(State::load(&fx.paths).unwrap().quarantine.contains_key(&stash));
}

#[test]
fn a_halted_refresh_is_not_attempted() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Ok(()));
    State::update(&fx.paths, |s| s.refresh_halted_until = Some(now_secs() + 3600)).unwrap();

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();

    assert!(err.contains("could not be matched to a saved account"), "{err}");
    assert_eq!(fx.profiles.refresh_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_rejected_client_halts_refreshes_and_the_switch_stops_as_before() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    *fx.profiles.refresh_reply.lock().unwrap() = Some(Err(RefreshError::Systemic("the token endpoint rejected agent-switch's client (invalid_client)".into())));

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();

    assert!(err.contains("could not be matched to a saved account") && err.contains("invalid_client"), "{err}");
    assert!(State::load(&fx.paths).unwrap().refresh_halted(now_secs()));
}

#[test]
fn a_transient_refresh_failure_is_named_in_the_refusal() {
    let fx = Fx::new();
    expire_live(&fx, 1);
    let live_before = fx.live_text();

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();

    assert!(err.contains("could not be matched to a saved account") && err.contains("401") && err.contains("HTTP 503"), "{err}");
    assert_eq!(fx.live_text(), live_before, "nothing was written to the live item");
}

#[test]
fn a_known_login_that_state_forgot_is_saved_into_its_own_slot() {
    let fx = Fx::new();
    // state.json says nothing is active, but the live login is team's (changed by hand earlier).
    let mut team_live = with(login("acc-T9", "ref-T1"), "mcpOAuth", json!({"m": 1}));
    team_live.insert("trustedDeviceToken".into(), json!("tdt-T"));
    fx.set_live(&team_live);
    fx.switch(&fx.adapter(), "personal").unwrap();
    assert_eq!(access(&fx.slot("team").login), "acc-T9", "saved to team's slot, not into personal's");
    assert_eq!(access(&fx.live_obj()), "acc-P1");
    assert_eq!(fx.live_obj()["mcpOAuth"], json!({"m": 1}));
}

// ---------------------------------------------------------------------------------------------
// Rollback with fault injection
// ---------------------------------------------------------------------------------------------

#[test]
fn a_failure_writing_the_live_item_changes_nothing() {
    let fx = Fx::new();
    let before = parse_login(&fx.live_text()).unwrap();
    let json_before = fx.claude_json();
    let team_before = fx.vault.get("team", STORE).unwrap();
    fx.live.fail_next_set.store(true, Ordering::SeqCst);

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();
    assert!(err.contains("rolled back") && err.contains("injected failure"), "{err}");
    assert_eq!(fx.live_obj(), before, "the live login is as it was");
    assert_eq!(fx.claude_json(), json_before);
    assert_eq!(fx.vault.get("team", STORE).unwrap(), team_before);
    assert_eq!(State::load(&fx.paths).unwrap(), State::default(), "a failed switch leaves no journal entry");
}

#[test]
fn a_missing_claude_json_stops_the_switch_before_the_keychain_is_written() {
    let fx = Fx::new();
    std::fs::remove_file(fx.claude_json_path()).unwrap();
    let before = fx.live_text();
    fx.live.fail_next_set.store(true, Ordering::SeqCst); // still armed afterwards means `set` was never called
    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();
    assert!(err.contains("does not exist") && !err.contains("rolled back"), "{err}");
    assert!(fx.live.fail_next_set.load(Ordering::SeqCst), "the Keychain must not be touched, not even to roll back");
    assert_eq!(fx.live_text(), before);
}

#[test]
fn a_symlinked_claude_json_or_credentials_file_stops_the_switch_untouched() {
    let fx = Fx::new();
    let managed = fx.paths.home().join("dotfiles-claude.json");
    std::fs::rename(fx.claude_json_path(), &managed).unwrap();
    std::os::unix::fs::symlink(&managed, fx.claude_json_path()).unwrap();
    let before = fx.live_text();
    fx.live.fail_next_set.store(true, Ordering::SeqCst);
    let err = fx.switch(&fx.adapter(), "team").unwrap_err();
    assert!(matches!(err, Error::SymlinkedFile(_)), "{err:?}");
    assert!(fx.live.fail_next_set.load(Ordering::SeqCst), "the Keychain must not be touched");
    assert_eq!(fx.live_text(), before);
    assert!(std::fs::symlink_metadata(fx.claude_json_path()).unwrap().file_type().is_symlink());

    // The same for a credentials file that Claude Code would read instead of the Keychain.
    let fx = Fx::new();
    let real = fx.paths.home().join("dotfiles-credentials.json");
    std::fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, fx.paths.home().join(".claude/.credentials.json")).unwrap();
    fx.live.fail_next_set.store(true, Ordering::SeqCst);
    assert!(matches!(fx.switch(&fx.adapter(), "team").unwrap_err(), Error::SymlinkedFile(_)));
    assert!(fx.live.fail_next_set.load(Ordering::SeqCst));
}

#[test]
fn a_failure_writing_the_identity_rolls_the_live_item_back() {
    let fx = Fx::new();
    let creds = fx.paths.home().join(".claude/.credentials.json");
    std::fs::write(&creds, "original-credentials-file").unwrap();
    let before = fx.live_obj();
    let json_before = fx.claude_json();
    let json_path = fx.claude_json_path();
    let writes = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let seen = writes.clone();
    let a = fx.adapter().with_file_writer(Arc::new(move |path, bytes| {
        seen.lock().unwrap().push(path.to_path_buf());
        if path == json_path {
            return Err(Error::Invalid("disk full".into()));
        }
        safe_write(path, bytes, &WriteOpts::secret())
    }));

    let err = fx.switch(&a, "team").unwrap_err().to_string();
    assert!(err.contains("rolled back") && err.contains("disk full"), "{err}");
    assert_eq!(fx.live_obj(), before, "the Keychain login was restored");
    assert_eq!(std::fs::read_to_string(&creds).unwrap(), "original-credentials-file", "the credentials file was restored");
    assert_eq!(fx.claude_json(), json_before);
    assert_eq!(State::load(&fx.paths).unwrap(), State::default());
    // The live write happened (creds file rewritten, identity attempted) before the rollback.
    assert!(writes.lock().unwrap().len() >= 3);
}

#[test]
fn a_login_that_does_not_fit_the_stdin_limit_fails_before_any_write() {
    let fx = Fx::new();
    let mut big = login("acc-T1", "ref-T1");
    big.insert("padding".into(), json!("x".repeat(4200)));
    fx.vault.put("team", STORE, &saved(big, "u-team")).unwrap();
    let (live_before, personal_before) = (fx.live_text(), fx.vault.get("personal", STORE).unwrap());

    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();
    assert!(err.contains("too large") && err.contains("Nothing was changed"), "{err}");
    assert_eq!(fx.live_text(), live_before);
    assert_eq!(fx.vault.get("personal", STORE).unwrap(), personal_before, "not even the save-back ran");
}

#[test]
fn pruning_drops_only_tokenless_mcp_entries() {
    let mut l = with(login("a", "r"), "mcpOAuth", json!({
        "stub": {"serverName": "s", "accessToken": ""},
        "null": {"accessToken": null, "refreshToken": null},
        "bare": {"serverName": "b"},
        "access": {"accessToken": "tok", "refreshToken": ""},
        "refresh": {"accessToken": "", "refreshToken": "rt"},
        "scalar": 1,
    }));
    assert!(prune_tokenless_mcp(&mut l));
    let kept: HashSet<&str> = l["mcpOAuth"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(kept, HashSet::from(["access", "refresh", "scalar"]));
    assert!(!prune_tokenless_mcp(&mut l), "nothing left to drop");

    let mut only_stubs = with(login("a", "r"), "mcpOAuth", json!({"stub": {"accessToken": ""}}));
    assert!(prune_tokenless_mcp(&mut only_stubs));
    assert_eq!(only_stubs["mcpOAuth"], json!({}), "an emptied mcpOAuth stays an empty object");
    assert!(!prune_tokenless_mcp(&mut login("a", "r")), "no mcpOAuth key");
}

#[test]
fn tokenless_mcp_stubs_are_dropped_only_when_the_login_would_not_fit() {
    let fx = Fx::new();
    let mut mcp = serde_json::Map::new();
    for i in 0..40 {
        mcp.insert(format!("plugin:x:srv{i}|{i:016x}"), json!({"serverName": "s", "serverUrl": "https://example.com/mcp", "accessToken": "", "discoveryState": {"pad": "y".repeat(80)}}));
    }
    mcp.insert("real|1".into(), json!({"serverName": "real", "accessToken": "mcp-tok", "refreshToken": "mcp-ref"}));
    let mut live = fx.live_obj();
    live.insert("mcpOAuth".into(), Value::Object(mcp));
    fx.set_live(&live);
    assert!(compact_ascii(&merge_login(Some(&live), &login("acc-T1", "ref-T1"))).unwrap().len() > 4090, "fixture must exceed the limit");

    fx.switch(&fx.adapter(), "team").unwrap();

    let after = fx.live_obj();
    assert_eq!(access(&after), "acc-T1");
    assert_eq!(after["mcpOAuth"], json!({"real|1": {"serverName": "real", "accessToken": "mcp-tok", "refreshToken": "mcp-ref"}}));
}

#[test]
fn unreadable_live_storage_fails_closed() {
    let fx = Fx::new();
    fx.live.set(LIVE_SERVICE, USER, "{\"somethingElse\":1}").unwrap();
    let err = fx.switch(&fx.adapter(), "team").unwrap_err().to_string();
    assert!(err.contains("unexpected format"), "{err}");
    assert_eq!(fx.live_text(), "{\"somethingElse\":1}");
}

// ---------------------------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------------------------

fn age_dir(dir: &Path, secs: u64) {
    let t = std::time::SystemTime::now() - Duration::from_secs(secs);
    std::fs::File::open(dir).unwrap().set_modified(t).unwrap();
}

#[test]
fn a_switch_holds_and_releases_claude_codes_locks() {
    let fx = Fx::new();
    let setup = Setup::resolve(&fx.paths, &fx.adapter().getenv);
    assert_eq!(setup.refresh_lock(), fx.paths.home().join(".claude/.oauth_refresh.lock"));
    assert_eq!(setup.legacy_lock(), fx.paths.home().join(".claude.lock"));
    assert_eq!(setup.config_lock(), fx.paths.home().join(".claude.json.lock"));
    fx.switch(&fx.adapter(), "team").unwrap();
    for l in [setup.refresh_lock(), setup.legacy_lock(), setup.config_lock()] {
        assert!(!l.exists(), "{} must be released", l.display());
    }
}

#[test]
fn stale_locks_from_a_dead_holder_are_taken_over() {
    let fx = Fx::new();
    let setup = Setup::resolve(&fx.paths, &fx.adapter().getenv);
    for l in [setup.refresh_lock(), setup.legacy_lock(), setup.config_lock()] {
        std::fs::create_dir(&l).unwrap();
        age_dir(&l, 120); // older than both the 60 s and 10 s limits
    }
    fx.switch(&fx.adapter(), "team").unwrap();
    assert_eq!(access(&fx.live_obj()), "acc-T1");
}

#[test]
fn a_live_holder_blocks_the_lock_until_the_timeout() {
    let fx = Fx::new();
    let setup = Setup::resolve(&fx.paths, &fx.adapter().getenv);
    let held = DirLock::acquire(&setup.refresh_lock(), DirLockOpts::credentials()).unwrap();
    let quick = DirLockOpts { timeout: Duration::from_millis(300), ..DirLockOpts::credentials() };
    let err = DirLock::acquire(&setup.refresh_lock(), quick).err().expect("contention");
    assert!(matches!(err, Error::LockBusy(_)), "{err}");
    // A 30 s old lock is still alive for the 60 s credentials rule.
    age_dir(&setup.refresh_lock(), 30);
    assert!(DirLock::acquire(&setup.refresh_lock(), quick).is_err());
    drop(held);
    assert!(DirLock::acquire(&setup.refresh_lock(), quick).is_ok());
}

// ---------------------------------------------------------------------------------------------
// current_identity and doctor
// ---------------------------------------------------------------------------------------------

#[test]
fn current_identity_matches_the_slot_and_cross_checks_the_config() {
    let fx = Fx::new();
    let a = fx.adapter();
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert_eq!((id.account_id.as_deref(), id.note), (Some("personal"), None));
    assert_eq!(id.email.as_deref(), Some("u-personal@example.com"));

    // The login is personal's but ~/.claude.json names team: split state.
    let mut cfg: Obj = serde_json::from_str(&fx.claude_json()).unwrap();
    cfg.insert("oauthAccount".into(), oauth_account("u-team"));
    std::fs::write(fx.claude_json_path(), serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert_eq!(id.account_id.as_deref(), Some("personal"));
    assert!(id.note.as_deref().unwrap().starts_with("split state"), "{:?}", id.note);
    let checks = a.doctor_checks(&fx.env());
    let split = checks.iter().find(|c| c.detail.contains("split state")).expect("doctor reports it");
    assert!(split.detail.contains("agent-switch switch claude-cli personal"), "{}", split.detail);
}

#[test]
fn current_identity_handles_logged_out_refreshed_and_unknown_logins() {
    let fx = Fx::new();
    let a = fx.adapter();
    let mut rotated = fx.live_obj();
    rotated.insert("claudeAiOauth".into(), json!({"accessToken": "a", "refreshToken": "ref-rotated", "expiresAt": 2}));
    fx.set_live(&rotated);
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert_eq!(id.account_id.as_deref(), Some("personal"), "found through oauthAccount after the token rotated");
    assert_eq!(id.note.as_deref(), Some("login refreshed since it was saved"));

    let mut cfg: Obj = serde_json::from_str(&fx.claude_json()).unwrap();
    cfg.insert("oauthAccount".into(), oauth_account("u-stranger"));
    std::fs::write(fx.claude_json_path(), serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert_eq!((id.account_id, id.note.as_deref()), (None, Some("login not saved in agent-switch")));
    assert!(a.doctor_checks(&fx.env()).iter().any(|c| c.level == Level::Warn && c.detail.contains("--from-keychain-item")));

    fx.live.delete(LIVE_SERVICE, USER).unwrap();
    assert_eq!(a.current_identity(&fx.env()).unwrap(), None);
}

#[test]
fn doctor_flags_a_format_it_cannot_parse() {
    let fx = Fx::new();
    fx.live.set(LIVE_SERVICE, USER, "{\"v2\":{}}").unwrap();
    let checks = fx.adapter().doctor_checks(&fx.env());
    assert!(checks.iter().any(|c| c.level == Level::Fail && c.detail.contains("unexpected format")), "{checks:?}");
    let healthy = Fx::new().adapter().doctor_checks(&Fx::new().env());
    assert!(healthy.iter().all(|c| c.level != Level::Fail));
}

// ---------------------------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------------------------

fn new_login_item(service: &str, fx: &Fx) -> LoginSource {
    let obj = with(login("acc-N1", "ref-N1"), "trustedDeviceToken", json!("tdt-N"));
    fx.live.set(service, USER, &compact_ascii(&obj).unwrap()).unwrap();
    fx.profiles.know("acc-N1", "u-new");
    LoginSource { service: service.into(), config_json: None }
}

#[test]
fn from_keychain_item_adds_an_account_and_is_idempotent() {
    let fx = Fx::new();
    let a = fx.adapter();
    let src = new_login_item("Claude Code-credentials-abcd1234", &fx);

    let dry = a.import_login(&fx.env(), &src, Some("new-acct"), None, true).unwrap();
    assert_eq!((dry.created, dry.dry_run, dry.plan.as_deref()), (true, true, Some("max")));
    assert!(fx.config().account("new-acct").is_none() && fx.vault.get("new-acct", STORE).unwrap().is_none(), "--dry-run writes nothing");

    let first = a.import_login(&fx.env(), &src, Some("new-acct"), Some("New"), false).unwrap();
    assert!(first.created);
    let acct = fx.config().account("new-acct").cloned().unwrap();
    assert_eq!((acct.uuid.as_deref(), acct.label.as_str(), acct.provider), (Some("u-new"), "New", Provider::Claude));
    let slot = fx.slot("new-acct");
    assert_eq!(slot.account_uuid(), Some("u-new"), "oauthAccount is synthesized from the profile when no .claude.json is given");
    assert_eq!(slot.login["trustedDeviceToken"], json!("tdt-N"));

    // Running it again refreshes the saved login and creates nothing.
    let again = a.import_login(&fx.env(), &src, Some("new-acct"), None, false).unwrap();
    assert!(!again.created);
    assert_eq!(fx.config().accounts.len(), 3);

    // The same Claude account under another id, or another account under this id, is refused.
    assert!(a.import_login(&fx.env(), &src, Some("other-id"), None, false).unwrap_err().to_string().contains("already saved as `new-acct`"));
    let team_src = LoginSource { service: "Claude Code-credentials-abcd1234".into(), config_json: None };
    fx.profiles.know("acc-N1", "u-team");
    assert!(a.import_login(&fx.env(), &team_src, Some("personal"), None, false).is_err());
}

#[test]
fn from_keychain_item_uses_the_config_dirs_identity_and_checks_the_hash() {
    let fx = Fx::new();
    let a = fx.adapter();
    let dir = fx.paths.home().join("claude-b");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join(".claude.json"), serde_json::to_string(&json!({"oauthAccount": oauth_account("u-new")})).unwrap()).unwrap();
    let service = keychain_service_name(dir.to_str().unwrap());
    new_login_item(&service, &fx);

    assert!(a.login_source(&fx.env(), "Claude Code-credentials-00000000", Some(&dir)).is_err(), "a dir that does not hash to the item is refused");
    let src = a.login_source(&fx.env(), &service, Some(&dir)).unwrap();
    a.import_login(&fx.env(), &src, Some("team-b"), None, false).unwrap();
    assert_eq!(fx.slot("team-b").oauth_account, oauth_account("u-new"), "the real oauthAccount is kept");

    // The default item reads ~/.claude.json.
    let default = a.login_source(&fx.env(), LIVE_SERVICE, None).unwrap();
    assert_eq!(default.config_json, Some(fx.claude_json_path()));

    // Profile and .claude.json disagreeing is a mixed identity.
    fx.profiles.know("acc-N1", "u-someone-else");
    assert!(a.import_login(&fx.env(), &src, Some("mixed"), None, false).unwrap_err().to_string().contains("mixed identity"));
    // If the profile endpoint is down, the .claude.json identity is enough.
    fx.profiles.by_token.lock().unwrap().clear();
    assert!(a.import_login(&fx.env(), &src, Some("team-b"), None, false).is_ok());
}

#[test]
fn account_ids_are_suggested_from_the_email() {
    assert_eq!(suggest_id(Some("Zee.Dev+x@corp.com"), "u"), "claude-zee-dev-x");
    assert_eq!(suggest_id(None, "abcdef123"), "claude-abcdef");
    assert!(crate::config::validate_account_id(&suggest_id(Some(&format!("{}@x.com", "a".repeat(60))), "u")).is_ok());
}

/// Any hashed `Claude Code-credentials-<hash>` item exists until it is deleted, like a profile that just logged in.
struct LoginAppears {
    deleted: Mutex<HashSet<String>>,
    value: String,
}

impl Keychain for LoginAppears {
    fn get(&self, service: &str, _account: &str) -> Result<Option<String>> {
        let hashed = service.starts_with("Claude Code-credentials-") && !self.deleted.lock().unwrap().contains(service);
        Ok(hashed.then(|| self.value.clone()))
    }
    fn set(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn delete(&self, service: &str, _: &str) -> Result<bool> {
        Ok(self.deleted.lock().unwrap().insert(service.to_string()))
    }
    fn name(&self) -> &'static str {
        "login-appears"
    }
}

#[test]
fn interactive_add_saves_the_new_login_and_cleans_up() {
    let fx = Fx::new();
    // A fake `claude` that behaves like a finished /login: it writes the profile's .claude.json.
    let script = fx.paths.home().join("fake-claude.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '{\"oauthAccount\":{\"accountUuid\":\"u-new\",\"emailAddress\":\"new@example.com\"}}' > \"$CLAUDE_CONFIG_DIR/.claude.json\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let kc = Arc::new(LoginAppears {
        deleted: Mutex::new(HashSet::new()),
        value: compact_ascii(&login("acc-N1", "ref-N1")).unwrap(),
    });
    fx.profiles.know("acc-N1", "u-new");
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("USER".into(), USER.into());
    let a = ClaudeCode::new(kc.clone(), fx.profiles.clone(), Arc::new(move |k| vars.get(k).cloned())).with_claude_binary(&script);

    let done = a.add_interactive(&fx.env(), Some("fresh"), None).unwrap();
    assert!(done.created);
    assert_eq!(fx.slot("fresh").oauth_account["emailAddress"], "new@example.com");
    assert_eq!(kc.deleted.lock().unwrap().len(), 1, "the temporary Keychain item was deleted");
    let tmp = fx.paths.support_dir().join("tmp");
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0, "the temporary profile folder was removed");

    // A failing `claude` with no login saves nothing and still cleans up.
    let empty = Arc::new(MemoryKeychain::new());
    let failing = ClaudeCode::new(empty.clone(), fx.profiles.clone(), Arc::new(|k| (k == "USER").then(|| USER.to_string()))).with_claude_binary("/usr/bin/false");
    assert!(failing.add_interactive(&fx.env(), Some("nope"), None).is_err());
    assert!(fx.config().account("nope").is_none());
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    assert!(empty.is_empty());
}

/// A `claude` stand-in: writes the profile's `.claude.json` like a finished /login, then runs `rest`.
fn fake_claude(fx: &Fx, rest: &str) -> PathBuf {
    let script = fx.paths.home().join("fake-claude.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '{{\"oauthAccount\":{{\"accountUuid\":\"u-new\",\"emailAddress\":\"new@example.com\"}}}}' > \"$CLAUDE_CONFIG_DIR/.claude.json\"\n{rest}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

fn adapter_over(fx: &Fx, kc: Arc<dyn Keychain>, claude: impl Into<PathBuf>) -> ClaudeCode {
    ClaudeCode::new(kc, fx.profiles.clone(), Arc::new(|k| (k == "USER").then(|| USER.to_string()))).with_claude_binary(claude)
}

#[test]
fn interactive_add_saves_the_login_while_claude_is_still_running() {
    let fx = Fx::new();
    let saved_marker = fx.paths.home().join("saved-while-running");
    // Claude waits for the account to show up in the config; it only does so if the save happened during the run.
    let rest = format!(
        "i=0\nwhile [ $i -lt 40 ]; do\n  if grep -q 'during' '{}' 2>/dev/null; then touch '{}'; break; fi\n  sleep 0.25; i=$((i+1))\ndone",
        fx.paths.config_file().display(),
        saved_marker.display()
    );
    let script = fake_claude(&fx, &rest);
    let kc = Arc::new(LoginAppears { deleted: Mutex::new(HashSet::new()), value: compact_ascii(&login("acc-N1", "ref-N1")).unwrap() });
    fx.profiles.know("acc-N1", "u-new");

    let done = adapter_over(&fx, kc.clone(), &script).add_interactive(&fx.env(), Some("during"), None).unwrap();
    assert!(done.created);
    assert!(saved_marker.exists(), "the account was saved before claude exited");
    assert_eq!(kc.deleted.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(fx.paths.support_dir().join("tmp")).unwrap().count(), 0);
}

#[test]
fn interactive_add_saves_the_login_after_claude_exits_with_an_error() {
    let fx = Fx::new();
    // The window was closed: claude ended on a signal-like status after the login existed.
    let script = fake_claude(&fx, "exit 129");
    let kc = Arc::new(LoginAppears { deleted: Mutex::new(HashSet::new()), value: compact_ascii(&login("acc-N1", "ref-N1")).unwrap() });
    fx.profiles.know("acc-N1", "u-new");

    let done = adapter_over(&fx, kc.clone(), &script).add_interactive(&fx.env(), Some("late"), None).unwrap();
    assert!(done.created);
    assert_eq!(fx.slot("late").oauth_account["emailAddress"], "new@example.com");
    assert_eq!(kc.deleted.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(fx.paths.support_dir().join("tmp")).unwrap().count(), 0);
}

#[test]
fn an_abandoned_profile_of_a_dead_process_is_saved_and_removed() {
    let fx = Fx::new();
    let tmp = fx.paths.support_dir().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let kc = Arc::new(MemoryKeychain::new());
    let profile = |name: &str, uuid: &str, access: &str| -> PathBuf {
        let dir = tmp.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".claude.json"), json!({"oauthAccount": oauth_account(uuid)}).to_string()).unwrap();
        kc.set(&keychain_service_name(dir.to_str().unwrap()), USER, &compact_ascii(&login(access, "ref-x")).unwrap()).unwrap();
        dir
    };
    let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
    gone.wait().unwrap();
    let dead = gone.id();
    let lost = profile(&format!("claude-add-{dead}-abc"), "u-lost", "acc-L1");
    let known = profile(&format!("claude-add-{dead}-def"), "u-team", "acc-K1");
    let running = profile(&format!("claude-add-{}-abc", std::process::id()), "u-running", "acc-R1");
    let unrelated = tmp.join("not-an-add-profile");
    std::fs::create_dir_all(&unrelated).unwrap();
    let target = fx.paths.home().join("link-target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep"), "x").unwrap();
    let link = tmp.join(format!("claude-add-{dead}-aaa"));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let link_item = keychain_service_name(link.to_str().unwrap());
    kc.set(&link_item, USER, &compact_ascii(&login("acc-S1", "ref-x")).unwrap()).unwrap();
    for (access, uuid) in [("acc-L1", "u-lost"), ("acc-K1", "u-team"), ("acc-R1", "u-running")] {
        fx.profiles.know(access, uuid);
    }
    kc.set(LIVE_SERVICE, USER, "live-login").unwrap();
    kc.set("Claude Code-credentials-00000000", USER, "other-profile").unwrap();
    let team_before = fx.vault.get("team", STORE).unwrap().unwrap().blob;

    // The new sign-in itself fails; recovery runs before it.
    assert!(adapter_over(&fx, kc.clone(), "/usr/bin/false").add_interactive(&fx.env(), None, None).is_err());

    let config = fx.config();
    let recovered = config.accounts.iter().find(|a| a.uuid.as_deref() == Some("u-lost")).expect("the lost login was saved");
    assert_eq!(fx.slot(&recovered.id).oauth_account["emailAddress"], "u-lost@example.com");
    assert!(!lost.exists() && !known.exists(), "dead-process folders are removed");
    assert!(kc.get(&keychain_service_name(lost.to_str().unwrap()), USER).unwrap().is_none());
    assert!(kc.get(&keychain_service_name(known.to_str().unwrap()), USER).unwrap().is_none());
    assert_eq!(fx.vault.get("team", STORE).unwrap().unwrap().blob, team_before, "an account that is already saved is not overwritten");
    assert!(config.accounts.iter().all(|a| a.uuid.as_deref() != Some("u-running")));
    assert!(running.exists() && unrelated.exists(), "a folder of a live process and unrelated folders stay");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink() && target.join("keep").exists(), "a link is never followed or removed");
    assert!(kc.get(&link_item, USER).unwrap().is_some());
    assert!(kc.get(&keychain_service_name(running.to_str().unwrap()), USER).unwrap().is_some());
    assert_eq!(kc.get(LIVE_SERVICE, USER).unwrap().as_deref(), Some("live-login"));
    assert_eq!(kc.get("Claude Code-credentials-00000000", USER).unwrap().as_deref(), Some("other-profile"));
}

/// Hashed item whose value switches to `rotated` once `marker` exists, like a login that claude refreshed.
struct Rotating {
    marker: PathBuf,
    first: String,
    rotated: String,
}

impl Keychain for Rotating {
    fn get(&self, service: &str, _account: &str) -> Result<Option<String>> {
        Ok(service.starts_with("Claude Code-credentials-").then(|| if self.marker.exists() { self.rotated.clone() } else { self.first.clone() }))
    }
    fn set(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn delete(&self, _: &str, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn name(&self) -> &'static str {
        "rotating"
    }
}

#[test]
fn a_login_that_claude_refreshed_after_the_save_replaces_the_saved_one_before_cleanup() {
    let fx = Fx::new();
    let marker = fx.paths.home().join("rotated");
    let rest = format!(
        "i=0\nwhile [ $i -lt 40 ]; do\n  if grep -q 'rot' '{}' 2>/dev/null; then break; fi\n  sleep 0.25; i=$((i+1))\ndone\ntouch '{}'\nexit 129",
        fx.paths.config_file().display(),
        marker.display()
    );
    let script = fake_claude(&fx, &rest);
    let kc = Arc::new(Rotating {
        marker,
        first: compact_ascii(&login("acc-N1", "ref-N1")).unwrap(),
        rotated: compact_ascii(&login("acc-N2", "ref-N2")).unwrap(),
    });
    fx.profiles.know("acc-N1", "u-new");
    fx.profiles.know("acc-N2", "u-new");

    let done = adapter_over(&fx, kc, &script).add_interactive(&fx.env(), Some("rot"), None).unwrap();
    assert!(done.created);
    let blob = fx.vault.get("rot", STORE).unwrap().unwrap().blob;
    assert!(blob.contains("ref-N2") && !blob.contains("ref-N1"), "the refreshed token is the saved one: {blob}");
    assert_eq!(fx.config().accounts.iter().filter(|a| a.uuid.as_deref() == Some("u-new")).count(), 1);
}

#[test]
fn recovery_picks_a_free_id_when_the_usual_one_belongs_to_another_account() {
    let fx = Fx::new();
    Config::update(&fx.paths, |c| {
        c.add_account(Account { id: "claude-u-lost".into(), provider: Provider::Claude, label: "x".into(), email: None, uuid: Some("u-other".into()) })
    })
    .unwrap();
    let tmp = fx.paths.support_dir().join("tmp");
    let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
    gone.wait().unwrap();
    let dir = tmp.join(format!("claude-add-{}-abc", gone.id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".claude.json"), json!({"oauthAccount": oauth_account("u-lost")}).to_string()).unwrap();
    let kc = Arc::new(MemoryKeychain::new());
    kc.set(&keychain_service_name(dir.to_str().unwrap()), USER, &compact_ascii(&login("acc-L1", "ref-x")).unwrap()).unwrap();
    fx.profiles.know("acc-L1", "u-lost");

    assert!(adapter_over(&fx, kc.clone(), "/usr/bin/false").add_interactive(&fx.env(), None, None).is_err());
    let config = fx.config();
    let recovered = config.accounts.iter().find(|a| a.uuid.as_deref() == Some("u-lost")).expect("recovered");
    assert_eq!(recovered.id, "claude-u-lost-2");
    assert!(!dir.exists() && kc.is_empty());
}

#[test]
fn the_explicit_id_of_the_current_add_wins_over_recovery() {
    let fx = Fx::new();
    let tmp = fx.paths.support_dir().join("tmp");
    let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
    gone.wait().unwrap();
    // An abandoned profile of the very same account that the current add signs in.
    let stale = tmp.join(format!("claude-add-{}-abc", gone.id()));
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join(".claude.json"), json!({"oauthAccount": oauth_account("u-new")}).to_string()).unwrap();
    let script = fake_claude(&fx, "exit 0");
    let kc = Arc::new(LoginAppears { deleted: Mutex::new(HashSet::new()), value: compact_ascii(&login("acc-N1", "ref-N1")).unwrap() });
    fx.profiles.know("acc-N1", "u-new");

    adapter_over(&fx, kc, &script).add_interactive(&fx.env(), Some("mine"), None).unwrap();
    let ids: Vec<String> = fx.config().accounts.iter().filter(|a| a.uuid.as_deref() == Some("u-new")).map(|a| a.id.clone()).collect();
    assert_eq!(ids, ["mine"]);
    assert!(!stale.exists(), "the leftover folder is still cleaned");
}

fn hangup_handler() -> libc::sighandler_t {
    // SAFETY: only reads the current disposition.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        assert_eq!(libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut action), 0);
        action.sa_sigaction
    }
}

#[test]
fn the_hangup_guard_handles_sighup_for_this_process_only_and_restores_it() {
    use std::os::unix::process::ExitStatusExt;
    // Hold the lock from before the first read, so no other test's guard changes the state in between.
    let lock = HANGUP_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = hangup_handler();
    {
        let _guard = HangupGuard::install_holding(lock);
        assert_eq!(hangup_handler(), ignore_hangup as extern "C" fn(libc::c_int) as libc::sighandler_t);
        // A child still dies on its own hangup: the handler is not inherited as "ignore".
        let status = Command::new("/bin/sh").args(["-c", "kill -HUP $$; sleep 5"]).status().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGHUP));
    }
    assert_eq!(hangup_handler(), before);
}

#[test]
fn only_add_profile_folder_names_carry_a_pid() {
    assert_eq!(add_profile_pid("claude-add-123-1a2b"), Some(123));
    for bad in ["claude-add-", "claude-add-x-1", "claude-add-12-", "claude-add-12-zz", "claude-add-0-1", "claude-add--1-1", "other-12-1", "add-account-claude.command"] {
        assert_eq!(add_profile_pid(bad), None, "{bad}");
    }
    assert!(pid_alive(std::process::id() as i32));
}

#[test]
fn bound_surface_with_a_stub_binding_still_works_through_the_trait_object() {
    // The registry hands out `Box<dyn Surface>`; make sure the adapter is usable that way.
    let fx = Fx::new();
    let boxed: Box<dyn Surface> = Box::new(fx.adapter());
    assert_eq!(boxed.kind(), SurfaceKind::ClaudeCli);
    let mut config = fx.config();
    config.bind(SurfaceKind::ClaudeCli, Binding::parse("team")).unwrap();
    switch::activate(&fx.env(), &config, boxed.as_ref(), "team").unwrap();
    assert_eq!(boxed.current_identity(&fx.env()).unwrap().unwrap().account_id.as_deref(), Some("team"));
}

#[test]
fn splice_replaces_only_the_oauth_account_value() {
    // Odd number spellings and escapes elsewhere must survive byte for byte; a decoy key sits inside a string
    // and inside a nested object.
    let text = "{\n  \"a\": 0.47586409723932444,\n  \"s\": \"x\\\" {\\\"oauthAccount\\\": 1}\",\n  \"nested\": {\"oauthAccount\": \"keep\"},\n  \"oauthAccount\": {\n    \"accountUuid\": \"old\"\n  },\n  \"z\": [1, 2e30]\n}\n";
    let out = splice_oauth_account(text, &json!({"accountUuid": "new", "n": 1})).unwrap();
    let want = text.replace("{\n    \"accountUuid\": \"old\"\n  }", "{\n    \"accountUuid\": \"new\",\n    \"n\": 1\n  }");
    assert_eq!(out, want);
    serde_json::from_str::<Value>(&out).unwrap();
}

#[test]
fn splice_adds_a_missing_key_in_the_files_layout() {
    let v = json!({"k": 1});
    let pretty = splice_oauth_account("{\n  \"a\": 1\n}", &v).unwrap();
    assert_eq!(pretty, "{\n  \"a\": 1,\n  \"oauthAccount\": {\n    \"k\": 1\n  }\n}");
    assert_eq!(splice_oauth_account("{\"a\":1}", &v).unwrap(), "{\"a\":1,\"oauthAccount\": {\"k\":1}}");
    assert_eq!(splice_oauth_account("{}", &v).unwrap(), "{\"oauthAccount\": {\"k\":1}}");
    assert_eq!(splice_oauth_account("{\n}\n", &v).unwrap(), "{\n  \"oauthAccount\": {\n    \"k\": 1\n  }\n}\n");
    assert!(splice_oauth_account("[1]", &v).is_err());
}

/// Evidence for the in-place identity rewrite on the real `~/.claude.json`: only the span of `oauthAccount`
/// changes. Run with `cargo test -p agent-switch-core claude_json_splice -- --ignored`.
#[test]
#[ignore = "reads the real ~/.claude.json"]
fn claude_json_splice_changes_only_the_identity_span() {
    let path = Paths::from_env().unwrap().home().join(".claude.json");
    let text = std::fs::read_to_string(path).unwrap();
    let original: Obj = serde_json::from_str(&text).unwrap();
    let mut changed = original["oauthAccount"].clone();
    changed["accountUuid"] = json!("spliced");
    let out = splice_oauth_account(&text, &changed).unwrap();
    let parsed: Obj = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["oauthAccount"]["accountUuid"], "spliced");
    let mut expected = original;
    expected.insert("oauthAccount".into(), changed);
    assert_eq!(parsed, expected);
    // Prefix and suffix around the span are byte-identical, including float spellings.
    let start = text.find("\"oauthAccount\"").unwrap();
    let end_marker = text[start..].find("\n  }").map(|e| start + e + 4).unwrap();
    assert!(out.starts_with(&text[..start]) && out.ends_with(&text[end_marker..]));
}
