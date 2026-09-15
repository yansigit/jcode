use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
const DEFAULT_AUTH_COOLDOWN: Duration = Duration::from_secs(300);
const DEFAULT_OTHER_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
const MAX_ERROR_KIND_CHARS: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedAccount {
    pub provider: String,
    pub account_id: String,
    pub label: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub source: String,
    #[serde(default)]
    pub active: bool,
}

/// Runtime health for an imported account.
///
/// This is deliberately persisted separately from [`ImportedAccount`]. Account
/// credentials are imported snapshots and must not be rewritten every time a
/// provider rejects a request. The state file contains no access or refresh
/// tokens, and failures are reduced to a closed vocabulary rather than storing
/// provider response bodies (which can contain credentials).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedAccountRuntimeState {
    #[serde(default)]
    pub cooldown_until_ms: Option<i64>,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_failure_kind: Option<String>,
    #[serde(default)]
    pub last_failure_at_ms: Option<i64>,
    #[serde(default)]
    pub last_success_at_ms: Option<i64>,
    #[serde(default)]
    pub last_selected_at_ms: Option<i64>,
}

fn path() -> Result<PathBuf> {
    Ok(crate::storage::app_config_dir()?
        .join("imported_auth")
        .join("account_pools.json"))
}

fn runtime_state_path() -> Result<PathBuf> {
    Ok(crate::storage::app_config_dir()?
        .join("imported_auth")
        .join("account_pool_state.json"))
}

// Account rotation can be triggered by more than one request task. Serialize
// read-modify-write operations so two failures cannot overwrite each other's
// cooldown or selection updates.
static RUNTIME_STATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn runtime_state_key(provider: &str, account_id: &str) -> String {
    format!("{provider}\n{account_id}")
}

fn load_runtime_states() -> BTreeMap<String, ImportedAccountRuntimeState> {
    let Ok(path) = runtime_state_path() else {
        return BTreeMap::new();
    };
    crate::storage::read_json(&path).unwrap_or_default()
}

fn save_runtime_states(states: &BTreeMap<String, ImportedAccountRuntimeState>) -> Result<()> {
    crate::storage::write_json(&runtime_state_path()?, states)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn cooldown_active(state: &ImportedAccountRuntimeState, now_ms: i64) -> bool {
    state.cooldown_until_ms.is_some_and(|until| until > now_ms)
}

/// An expired access token is still usable when the provider supplied a
/// refresh token. Only an expired account with no refresh credential is
/// ineligible for automatic use.
fn expired_without_refresh(account: &ImportedAccount, now_ms: i64) -> bool {
    account
        .expires_at
        .is_some_and(|expires_at| expires_at <= now_ms)
        && account
            .refresh_token
            .as_deref()
            .is_none_or(|refresh| refresh.trim().is_empty())
}

fn failure_kind(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if [
        "unauthorized",
        "unauthorised",
        "authentication",
        "not logged in",
        "not_login",
        "invalid token",
        "token expired",
        "access denied",
        "forbidden",
        "401",
        "403",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "authentication"
    } else if [
        "rate limit",
        "rate_limit",
        "rate-limit",
        "too many requests",
        "resource exhausted",
        "quota",
        "429",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "rate_limited"
    } else {
        "other"
    }
}

/// Whether a provider failure is safe to retry once with another imported
/// account. This deliberately excludes malformed requests, network errors, and
/// other failures that are not account-specific.
pub fn is_rotatable_error(error: &str) -> bool {
    matches!(failure_kind(error), "authentication" | "rate_limited")
}

fn cooldown_for(kind: &str, failures: u32) -> Duration {
    let base = match kind {
        "authentication" => DEFAULT_AUTH_COOLDOWN,
        "rate_limited" => DEFAULT_RATE_LIMIT_COOLDOWN,
        _ => DEFAULT_OTHER_COOLDOWN,
    };
    let multiplier = 1u32
        .checked_shl(failures.saturating_sub(1).min(10))
        .unwrap_or(u32::MAX);
    base.checked_mul(multiplier)
        .unwrap_or(MAX_FAILURE_BACKOFF)
        .min(MAX_FAILURE_BACKOFF)
}

/// Return the persisted runtime state for one imported account, if present.
pub fn runtime_state(provider: &str, account_id: &str) -> Option<ImportedAccountRuntimeState> {
    load_runtime_states()
        .get(&runtime_state_key(provider, account_id))
        .cloned()
}

/// Return the next account that is not cooling down, excluding the failed or
/// currently-used account when requested. Least-recently-selected accounts win
/// so concurrent turns do not repeatedly choose the first alternate.
pub fn next_eligible_account(
    provider: &str,
    exclude_account_id: Option<&str>,
) -> Option<ImportedAccount> {
    let now = now_ms();
    let states = load_runtime_states();
    list_provider(provider)
        .into_iter()
        .enumerate()
        .filter(|(_, account)| {
            exclude_account_id != Some(account.account_id.as_str())
                && !expired_without_refresh(account, now)
                && !cooldown_active(
                    states
                        .get(&runtime_state_key(provider, &account.account_id))
                        .unwrap_or(&ImportedAccountRuntimeState::default()),
                    now,
                )
        })
        .min_by_key(|(index, account)| {
            (
                states
                    .get(&runtime_state_key(provider, &account.account_id))
                    .and_then(|state| state.last_selected_at_ms)
                    .unwrap_or_default(),
                *index,
            )
        })
        .map(|(_, account)| account)
}

/// Prefer the explicitly active imported account when it is healthy; otherwise
/// return the least-recently-selected account outside its cooldown. This keeps
/// manual account switches authoritative while preventing a persisted failure
/// from pinning every later request to the same account.
pub fn active_or_next_eligible_account(provider: &str) -> Option<ImportedAccount> {
    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = now_ms();
    let states = load_runtime_states();
    let accounts = list_provider(provider);
    if let Some(account) = accounts.iter().find(|account| {
        account.active
            && !expired_without_refresh(account, now)
            && !cooldown_active(
                states
                    .get(&runtime_state_key(provider, &account.account_id))
                    .unwrap_or(&ImportedAccountRuntimeState::default()),
                now,
            )
    }) {
        return Some(account.clone());
    }

    let next = accounts
        .into_iter()
        .enumerate()
        .filter(|(_, account)| !expired_without_refresh(account, now))
        .filter(|(_, account)| {
            !cooldown_active(
                states
                    .get(&runtime_state_key(provider, &account.account_id))
                    .unwrap_or(&ImportedAccountRuntimeState::default()),
                now,
            )
        })
        .min_by_key(|(index, account)| {
            (
                states
                    .get(&runtime_state_key(provider, &account.account_id))
                    .and_then(|state| state.last_selected_at_ms)
                    .unwrap_or_default(),
                *index,
            )
        })
        .map(|(_, account)| account);
    if let Some(account) = &next {
        set_active_unlocked(provider, &account.account_id).ok()?;
    }
    next
}

/// Record a provider failure without persisting the provider's error body.
/// Returns the updated state so callers can include safe status in their own
/// control flow without ever handling or logging a token.
pub fn record_failure(
    provider: &str,
    account_id: &str,
    error: &str,
) -> Result<ImportedAccountRuntimeState> {
    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut states = load_runtime_states();
    let key = runtime_state_key(provider, account_id);
    let mut state = states.remove(&key).unwrap_or_default();
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    let kind = failure_kind(error);
    let cooldown = cooldown_for(kind, state.consecutive_failures);
    let now = now_ms();
    state.cooldown_until_ms = Some(now.saturating_add(cooldown.as_millis() as i64));
    state.last_failure_kind = Some(kind.chars().take(MAX_ERROR_KIND_CHARS).collect());
    state.last_failure_at_ms = Some(now);
    states.insert(key, state.clone());
    save_runtime_states(&states)?;
    Ok(state)
}

/// Clear a failed account's cooldown after a successful request.
pub fn record_success(provider: &str, account_id: &str) -> Result<()> {
    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut states = load_runtime_states();
    let key = runtime_state_key(provider, account_id);
    let mut state = states.remove(&key).unwrap_or_default();
    state.cooldown_until_ms = None;
    state.consecutive_failures = 0;
    state.last_failure_kind = None;
    state.last_success_at_ms = Some(now_ms());
    states.insert(key, state);
    save_runtime_states(&states)
}

/// Mark an account failed and atomically select and activate the next eligible
/// imported account. The selected account is left active so a successful retry
/// naturally becomes the account used by subsequent requests.
pub fn rotate_to_next_account(
    provider: &str,
    failed_account_id: &str,
    error: &str,
) -> Result<Option<ImportedAccount>> {
    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut states = load_runtime_states();
    let key = runtime_state_key(provider, failed_account_id);
    let mut failed = states.remove(&key).unwrap_or_default();
    failed.consecutive_failures = failed.consecutive_failures.saturating_add(1);
    let kind = failure_kind(error);
    let now = now_ms();
    failed.cooldown_until_ms = Some(
        now.saturating_add(cooldown_for(kind, failed.consecutive_failures).as_millis() as i64),
    );
    failed.last_failure_kind = Some(kind.to_string());
    failed.last_failure_at_ms = Some(now);
    states.insert(key, failed);

    let next = list_provider(provider)
        .into_iter()
        .enumerate()
        .filter(|(_, account)| account.account_id != failed_account_id)
        .filter(|(_, account)| !expired_without_refresh(account, now))
        .filter(|(_, account)| {
            !cooldown_active(
                states
                    .get(&runtime_state_key(provider, &account.account_id))
                    .unwrap_or(&ImportedAccountRuntimeState::default()),
                now,
            )
        })
        .min_by_key(|(index, account)| {
            (
                states
                    .get(&runtime_state_key(provider, &account.account_id))
                    .and_then(|state| state.last_selected_at_ms)
                    .unwrap_or_default(),
                *index,
            )
        })
        .map(|(_, account)| account);

    if let Some(account) = &next {
        let selected_key = runtime_state_key(provider, &account.account_id);
        let selected = states.entry(selected_key).or_default();
        selected.last_selected_at_ms = Some(now);
        // Keep the active-account update in the same serialized critical
        // section as the state update. `set_active` rereads the credential
        // snapshot but does not expose its token in errors or logs.
        set_active_unlocked(provider, &account.account_id)?;
    }
    save_runtime_states(&states)?;
    Ok(next)
}

pub fn import_opencodex_accounts(value: &Value) -> Result<Vec<ImportedAccount>> {
    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut accounts = Vec::new();
    let Some(root) = value.as_object() else {
        return Ok(accounts);
    };
    for (provider, provider_value) in root {
        let Some(provider_object) = provider_value.as_object() else {
            continue;
        };
        let Some(entries) = provider_object.get("accounts").and_then(Value::as_array) else {
            continue;
        };
        for (index, entry) in entries.iter().enumerate() {
            let Some(object) = entry.as_object() else {
                continue;
            };
            let Some(credential) = object.get("credential").and_then(Value::as_object) else {
                continue;
            };
            let Some(access) = credential
                .get("access")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            let account_id = object
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| credential.get("accountId").and_then(Value::as_str))
                .unwrap_or("")
                .trim();
            let account_id = if account_id.is_empty() {
                format!("{provider}-{index}")
            } else {
                account_id.to_string()
            };
            let active = provider_object
                .get("activeAccountId")
                .and_then(Value::as_str)
                .is_some_and(|id| id == account_id);
            let label = object
                .get("alias")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("{provider}-{index}"));
            accounts.push(ImportedAccount {
                provider: provider.clone(),
                account_id,
                label,
                access_token: access.to_string(),
                refresh_token: credential
                    .get("refresh")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(ToOwned::to_owned),
                expires_at: credential.get("expires").and_then(Value::as_i64),
                source: "open-codex".to_string(),
                active,
            });
        }
    }
    let existing = list();
    let mut preserved_active = BTreeMap::new();
    let mut existing_imports = BTreeMap::new();
    for account in existing
        .iter()
        .filter(|account| account.source == "open-codex")
    {
        existing_imports.insert(
            (account.provider.clone(), account.account_id.clone()),
            account,
        );
        if account.active {
            preserved_active.insert(account.provider.clone(), account.account_id.clone());
        }
    }

    // Replace the Open-Codex snapshot with the latest source contents, while
    // keeping a user's managed selection when that account still exists. Do
    // not let a source file's activeAccountId silently undo a local switch.
    for account in &mut accounts {
        if let Some(existing) =
            existing_imports.get(&(account.provider.clone(), account.account_id.clone()))
        {
            let managed_credentials_are_newer = match (existing.expires_at, account.expires_at) {
                (Some(managed), Some(incoming)) => managed > incoming,
                (Some(_), None) => true,
                _ => false,
            };
            if managed_credentials_are_newer {
                account.access_token.clone_from(&existing.access_token);
                if existing.refresh_token.is_some() {
                    account.refresh_token.clone_from(&existing.refresh_token);
                }
                account.expires_at = existing.expires_at;
            }
        }
        if let Some(active_id) = preserved_active.get(&account.provider) {
            account.active = &account.account_id == active_id;
        }
    }
    let mut merged: Vec<ImportedAccount> = existing
        .into_iter()
        .filter(|account| account.source != "open-codex")
        .collect();
    merged.extend(accounts);

    let target = path()?;
    crate::storage::write_json_secret(&target, &merged)?;
    Ok(merged)
}

pub fn list() -> Vec<ImportedAccount> {
    let Ok(target) = path() else {
        return Vec::new();
    };
    crate::storage::read_json(&target).unwrap_or_default()
}

pub fn list_provider(provider: &str) -> Vec<ImportedAccount> {
    list()
        .into_iter()
        .filter(|account| account.provider == provider)
        .collect()
}

fn set_active_unlocked(provider: &str, label: &str) -> Result<()> {
    let target = path()?;
    let mut accounts: Vec<ImportedAccount> = crate::storage::read_json(&target)?;
    let mut found = false;
    for account in &mut accounts {
        if account.provider == provider {
            account.active = account.label == label || account.account_id == label;
            found |= account.active;
        }
    }
    if !found {
        anyhow::bail!("No imported {provider} account named '{label}'")
    }
    crate::storage::write_json_secret(&target, &accounts)?;
    Ok(())
}

pub fn set_active(provider: &str, label: &str) -> Result<()> {
    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_active_unlocked(provider, label)
}

/// Persist refreshed credentials for one imported account without changing
/// its active selection or runtime health state. `None` keeps the previous
/// refresh token or expiry, which is important for OAuth responses that omit a
/// refresh token on subsequent refreshes.
pub fn update_tokens(
    provider: &str,
    account_id: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<i64>,
) -> Result<()> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        anyhow::bail!("Imported {provider} account {account_id} returned an empty access token")
    }

    let _guard = RUNTIME_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let target = path()?;
    let mut accounts: Vec<ImportedAccount> = crate::storage::read_json(&target)?;
    let Some(account) = accounts
        .iter_mut()
        .find(|account| account.provider == provider && account.account_id == account_id)
    else {
        anyhow::bail!("No imported {provider} account with id '{account_id}'")
    };

    account.access_token = access_token.to_string();
    if let Some(refresh_token) = refresh_token
        .map(str::trim)
        .filter(|refresh_token| !refresh_token.is_empty())
    {
        account.refresh_token = Some(refresh_token.to_string());
    }
    if let Some(expires_at) = expires_at {
        account.expires_at = Some(expires_at);
    }
    crate::storage::write_json_secret(&target, &accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    struct TestHome {
        _temp: TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let temp = tempdir().unwrap();
            let previous = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", temp.path());
            Self {
                _temp: temp,
                previous,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }

    #[test]
    fn imports_every_nested_provider_account() {
        let _env_lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let value = json!({
            "cursor": {"activeAccountId": "cursor-b", "accounts": [
                {"id": "cursor-a", "alias": "first", "credential": {"access": "a", "refresh": "ra", "expires": 1}},
                {"id": "cursor-b", "alias": "second", "credential": {"access": "b", "refresh": "rb", "expires": 2}}
            ]},
            "google-antigravity": {"accounts": [
                {"id": "ag-a", "credential": {"access": "c", "refresh": "rc", "expires": 3}}
            ]}
        });
        let accounts = import_opencodex_accounts(&value).unwrap();
        assert_eq!(accounts.len(), 3);
        assert_eq!(accounts[1].account_id, "cursor-b");
        assert_eq!(accounts[1].label, "second");
        assert_eq!(accounts[2].provider, "google-antigravity");
    }

    #[test]
    fn skips_accounts_without_access_tokens() {
        let _env_lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let value = json!({"cursor": {"accounts": [
            {"id": "missing", "credential": {"refresh": "r"}},
            {"id": "valid", "credential": {"access": "a"}}
        ]}});
        let accounts = import_opencodex_accounts(&value).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "valid");
    }

    #[test]
    fn failure_classification_and_backoff_are_provider_neutral() {
        assert_eq!(failure_kind("HTTP 401 unauthorized"), "authentication");
        assert_eq!(failure_kind("HTTP 429 resource exhausted"), "rate_limited");
        assert_eq!(failure_kind("connection reset"), "other");
        assert!(is_rotatable_error("HTTP 401 unauthorized"));
        assert!(is_rotatable_error("HTTP 429 resource exhausted"));
        assert!(!is_rotatable_error("malformed request"));
        assert_eq!(cooldown_for("rate_limited", 1), DEFAULT_RATE_LIMIT_COOLDOWN);
        assert_eq!(cooldown_for("authentication", 2), Duration::from_secs(600));
        assert_eq!(cooldown_for("authentication", 99), MAX_FAILURE_BACKOFF);
    }

    #[test]
    fn expired_accounts_need_refresh_credentials_to_remain_eligible() {
        let now = now_ms();
        let expired_no_refresh_account = ImportedAccount {
            provider: "cursor".to_string(),
            account_id: "expired-no-refresh".to_string(),
            label: "expired-no-refresh".to_string(),
            access_token: "access".to_string(),
            refresh_token: None,
            expires_at: Some(now - 1),
            source: "open-codex".to_string(),
            active: false,
        };
        let expired_with_refresh = ImportedAccount {
            refresh_token: Some("refresh".to_string()),
            account_id: "expired-refreshable".to_string(),
            label: "expired-refreshable".to_string(),
            ..expired_no_refresh_account.clone()
        };
        assert!(expired_without_refresh(&expired_no_refresh_account, now));
        assert!(!expired_without_refresh(&expired_with_refresh, now));
    }

    #[test]
    fn refresh_preserves_the_locally_selected_imported_account() {
        let _env_lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let initial = json!({
            "cursor": {"activeAccountId": "cursor-a", "accounts": [
                {"id": "cursor-a", "credential": {"access": "a", "refresh": "ra"}},
                {"id": "cursor-b", "credential": {"access": "b", "refresh": "rb"}}
            ]}
        });
        import_opencodex_accounts(&initial).unwrap();
        set_active("cursor", "cursor-b").unwrap();

        let refreshed_source = json!({
            "cursor": {"activeAccountId": "cursor-a", "accounts": [
                {"id": "cursor-a", "credential": {"access": "a2", "refresh": "ra2"}},
                {"id": "cursor-b", "credential": {"access": "b2", "refresh": "rb2"}}
            ]}
        });
        import_opencodex_accounts(&refreshed_source).unwrap();
        let accounts = list_provider("cursor");
        assert!(
            accounts
                .iter()
                .any(|account| account.account_id == "cursor-b" && account.active)
        );
        assert!(
            !accounts
                .iter()
                .any(|account| account.account_id == "cursor-a" && account.active)
        );
    }

    #[test]
    fn source_merge_preserves_newer_locally_refreshed_credentials() {
        let _env_lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let initial = json!({
            "cursor": {"activeAccountId": "cursor-a", "accounts": [
                {"id": "cursor-a", "credential": {
                    "access": "source-old", "refresh": "source-refresh", "expires": 2_000
                }}
            ]}
        });
        import_opencodex_accounts(&initial).unwrap();
        update_tokens(
            "cursor",
            "cursor-a",
            "managed-new",
            Some("managed-refresh"),
            Some(9_000),
        )
        .unwrap();

        let changed_source = json!({
            "cursor": {"activeAccountId": "cursor-a", "accounts": [
                {"id": "cursor-a", "credential": {
                    "access": "source-old", "refresh": "source-refresh", "expires": 2_000
                }},
                {"id": "cursor-b", "credential": {
                    "access": "source-b", "refresh": "source-refresh-b", "expires": 3_000
                }}
            ]}
        });
        import_opencodex_accounts(&changed_source).unwrap();

        let account = list_provider("cursor")
            .into_iter()
            .find(|account| account.account_id == "cursor-a")
            .unwrap();
        assert_eq!(account.access_token, "managed-new");
        assert_eq!(account.refresh_token.as_deref(), Some("managed-refresh"));
        assert_eq!(account.expires_at, Some(9_000));
        assert!(account.active);
    }

    #[test]
    fn persists_failure_state_without_credentials_and_rotates_eligible_account() {
        let _env_lock = crate::storage::lock_test_env();
        let _home = TestHome::new();

        let value = json!({
            "cursor": {"activeAccountId": "cursor-a", "accounts": [
                {"id": "cursor-a", "alias": "first", "credential": {"access": "access-secret-a", "refresh": "refresh-secret-a"}},
                {"id": "cursor-b", "alias": "second", "credential": {"access": "access-secret-b", "refresh": "refresh-secret-b"}}
            ]}
        });
        import_opencodex_accounts(&value).unwrap();

        let state = record_failure(
            "cursor",
            "cursor-a",
            "HTTP 401 unauthorized bearer access-secret-a",
        )
        .unwrap();
        assert_eq!(state.last_failure_kind.as_deref(), Some("authentication"));
        assert!(state.cooldown_until_ms.unwrap() > now_ms());
        update_tokens(
            "cursor",
            "cursor-a",
            "access-secret-a-refreshed",
            None,
            Some(now_ms() + 3_600_000),
        )
        .unwrap();
        let refreshed = list_provider("cursor")
            .into_iter()
            .find(|account| account.account_id == "cursor-a")
            .unwrap();
        assert_eq!(refreshed.access_token, "access-secret-a-refreshed");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-secret-a"));
        assert!(refreshed.active);
        assert_eq!(runtime_state("cursor", "cursor-a").unwrap(), state);
        assert_eq!(
            next_eligible_account("cursor", Some("cursor-a"))
                .unwrap()
                .account_id,
            "cursor-b"
        );
        assert_eq!(
            active_or_next_eligible_account("cursor")
                .unwrap()
                .account_id,
            "cursor-b"
        );
        assert!(
            list_provider("cursor")
                .iter()
                .any(|account| account.account_id == "cursor-b" && account.active)
        );

        let state_path = runtime_state_path().unwrap();
        let raw_state = std::fs::read_to_string(state_path).unwrap();
        assert!(!raw_state.contains("access-secret-a"));
        assert!(!raw_state.contains("refresh-secret-a"));

        record_success("cursor", "cursor-a").unwrap();
        assert!(
            runtime_state("cursor", "cursor-a")
                .unwrap()
                .cooldown_until_ms
                .is_none()
        );
        set_active("cursor", "first").unwrap();
        assert_eq!(
            rotate_to_next_account("cursor", "cursor-a", "HTTP 429 rate limit")
                .unwrap()
                .unwrap()
                .account_id,
            "cursor-b"
        );
        assert!(
            list_provider("cursor")
                .iter()
                .any(|account| account.account_id == "cursor-b" && account.active)
        );
    }
}
