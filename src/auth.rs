use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod codex;

/// A provider `ol login` can paste a key into, and what to call it while
/// asking. Signing in to a subscription is not one of these — it has no key
/// to type, so it lives in [`codex`].
pub struct ApiKeyLogin {
    /// The name the key is filed under, which is the provider's name in the
    /// config.
    pub provider: &'static str,
    pub display_name: &'static str,
    pub env_var: &'static str,
}

pub fn api_key_login(name: &str) -> Option<ApiKeyLogin> {
    match name {
        "openrouter" => Some(ApiKeyLogin {
            provider: "openrouter",
            display_name: "OpenRouter",
            env_var: "OPENROUTER_API_KEY",
        }),
        _ => None,
    }
}

/// One stored credential. Which kind it is is the `type` the file itself
/// carries, not the name it is filed under — a provider is free to change
/// how it lets callers in, and several already have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    /// A key, pasted once.
    ApiKey { key: String },
    /// A grant, renewed as it expires.
    Oauth(OauthEntry),
}

/// An OAuth grant: a short-lived access token, the refresh token that renews
/// it, and the account the tokens speak for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthEntry {
    pub access: String,
    pub refresh: String,
    /// Unix seconds. Renewed a minute early, so a request never starts on a
    /// token that expires while it is in flight.
    pub expires: i64,
    /// Which account the subscription belongs to; the backend wants it in a
    /// header of its own.
    pub account_id: String,
}

/// `~/.oneloop/auth.json`: one entry per provider, filed under the name that
/// provider has in the config.
///
/// Held as raw JSON rather than as typed fields, and typed only on the way
/// in and out. This version is not the only thing that has ever written the
/// file — an entry it cannot read is one another version stored, and the
/// file is rewritten on every token refresh, so anything dropped on a read
/// would be deleted on the next write.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthFile {
    entries: BTreeMap<String, Value>,
}

impl AuthFile {
    /// `None` covers both "no entry" and "an entry this version cannot
    /// read" — neither is a credential it can use, and the difference
    /// matters only to the write path, which never drops either.
    fn credential(&self, provider: &str) -> Option<Credential> {
        serde_json::from_value(self.entries.get(provider)?.clone()).ok()
    }

    fn set(&mut self, provider: &str, credential: &Credential) -> Result<()> {
        let value = serde_json::to_value(credential).context("failed to encode the credential")?;
        self.entries.insert(provider.to_string(), value);
        Ok(())
    }
}

/// Credentials loaded once from `~/.oneloop/auth.json`.
/// A missing or unreadable file behaves as empty.
pub struct Auth {
    file: AuthFile,
}

pub fn load() -> Auth {
    Auth {
        file: load_auth_file().unwrap_or_default(),
    }
}

impl Auth {
    /// The key for a provider: the environment variable it names, then what
    /// is stored under its name. An explicitly set env var wins — setting
    /// one is the caller saying "use this", per the usual CLI convention.
    /// Empty env values are ignored.
    pub fn api_key(&self, provider: &str, env_var: Option<&str>) -> Option<String> {
        env_var
            .and_then(|var| env::var(var).ok())
            .filter(|key| !key.trim().is_empty())
            .or_else(|| match self.file.credential(provider)? {
                Credential::ApiKey { key } => Some(key),
                Credential::Oauth(_) => None,
            })
    }

    /// The stored grant for a provider, if signing in to it has ever run.
    pub fn grant(&self, provider: &str) -> Option<OauthEntry> {
        match self.file.credential(provider)? {
            Credential::Oauth(grant) => Some(grant),
            Credential::ApiKey { .. } => None,
        }
    }
}

/// Written on login, and again on every refresh: the file is read back
/// before it is written, so an entry another version left there survives a
/// credential this one stores.
pub fn store(provider: &str, credential: &Credential) -> Result<PathBuf> {
    let mut auth = load_auth_file().unwrap_or_default();
    auth.set(provider, credential)?;
    write_auth_file(&auth)
}

fn auth_file_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".oneloop").join("auth.json"))
}

fn write_auth_file(auth: &AuthFile) -> Result<PathBuf> {
    let path = auth_file_path()?;
    let dir = path
        .parent()
        .context("auth file path has no parent directory")?;
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create auth directory: {}", dir.display()))?;

    let json = serde_json::to_string_pretty(auth).context("failed to serialize auth file")?;

    // API keys are secrets: create owner-only, and tighten permissions on
    // files written by older versions that used the default umask.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to open auth file: {}", path.display()))?;
    file.write_all(json.as_bytes())
        .with_context(|| format!("failed to write auth file: {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set auth file permissions: {}", path.display()))?;
    Ok(path)
}

fn load_auth_file() -> Result<AuthFile> {
    let path = auth_file_path()?;
    if !path.exists() {
        return Ok(AuthFile::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read auth file: {}", path.display()))?;
    let auth = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse auth file: {}", path.display()))?;
    Ok(auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(json: &str) -> AuthFile {
        serde_json::from_str(json).expect("test json must parse")
    }

    fn auth(json: &str) -> Auth {
        Auth { file: file(json) }
    }

    #[test]
    fn env_var_beats_stored_key_and_blank_env_is_ignored() {
        let auth = auth(r#"{"openrouter": {"type": "api_key", "key": "stored"}}"#);
        let key = || auth.api_key("openrouter", Some("OPENROUTER_API_KEY"));

        // SAFETY: no other test reads or writes this variable concurrently.
        unsafe { env::set_var("OPENROUTER_API_KEY", "from-env") };
        assert_eq!(key().as_deref(), Some("from-env"));

        // SAFETY: as above.
        unsafe { env::set_var("OPENROUTER_API_KEY", "  ") };
        assert_eq!(key().as_deref(), Some("stored"));

        // SAFETY: as above.
        unsafe { env::remove_var("OPENROUTER_API_KEY") };
        assert_eq!(key().as_deref(), Some("stored"));
    }

    /// A provider that names no variable still finds what was stored under
    /// its name — a subscription has no key to put in a variable.
    #[test]
    fn a_provider_without_a_variable_still_reads_its_entry() {
        let auth = auth(r#"{"p": {"type": "api_key", "key": "stored"}}"#);
        assert_eq!(auth.api_key("p", None).as_deref(), Some("stored"));
    }

    /// The `type` decides what an entry is, so asking for the wrong kind
    /// gets nothing rather than a credential that cannot be used that way.
    #[test]
    fn an_entry_is_only_the_kind_its_type_says() {
        let auth = auth(
            r#"{
                "key-provider":   {"type": "api_key", "key": "k"},
                "grant-provider": {"type": "oauth", "access": "a", "refresh": "r",
                                   "expires": 1, "account_id": "acct"}
            }"#,
        );
        assert_eq!(auth.api_key("key-provider", None).as_deref(), Some("k"));
        assert!(auth.grant("key-provider").is_none());

        assert_eq!(auth.grant("grant-provider").unwrap().account_id, "acct");
        assert!(auth.api_key("grant-provider", None).is_none());
    }

    /// A refresh rewrites the whole file, so an entry this version cannot
    /// read has to survive the round trip: it is another version's login,
    /// and dropping it deletes it. The one without a `type` is not
    /// hypothetical — it is what an older OneLoop wrote.
    #[test]
    fn entries_this_version_cannot_read_survive_a_rewrite() {
        let mut file = file(
            r#"{
                "openrouter":      {"type": "api_key", "key": "k"},
                "oauth_anthropic": {"access_token": "a", "refresh_token": "r", "expiry": 1},
                "half-an-oauth":   {"type": "oauth", "refresh": "r"}
            }"#,
        );
        file.set(
            "openai",
            &Credential::Oauth(OauthEntry {
                access: "a".to_string(),
                refresh: "r".to_string(),
                expires: 1,
                account_id: "acct".to_string(),
            }),
        )
        .unwrap();
        let written = serde_json::to_value(&file).unwrap();

        assert_eq!(written["oauth_anthropic"]["refresh_token"], "r");
        assert_eq!(written["half-an-oauth"]["refresh"], "r");
        assert_eq!(written["openrouter"]["key"], "k");
        assert_eq!(written["openai"]["type"], "oauth");
        assert_eq!(written["openai"]["account_id"], "acct");
    }
}
