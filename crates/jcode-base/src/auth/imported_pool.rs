use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedAccount {
    pub provider: String,
    pub account_id: String,
    pub label: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub source: String,
}

fn path() -> Result<PathBuf> {
    Ok(crate::storage::app_config_dir()?
        .join("imported_auth")
        .join("account_pools.json"))
}

pub fn import_opencodex_accounts(value: &Value) -> Result<Vec<ImportedAccount>> {
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
            });
        }
    }
    let target = path()?;
    crate::storage::write_json_secret(&target, &accounts)?;
    Ok(accounts)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn imports_every_nested_provider_account() {
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
        let value = json!({"cursor": {"accounts": [
            {"id": "missing", "credential": {"refresh": "r"}},
            {"id": "valid", "credential": {"access": "a"}}
        ]}});
        let accounts = import_opencodex_accounts(&value).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "valid");
    }
}
