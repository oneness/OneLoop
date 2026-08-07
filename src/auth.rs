use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The one vendor that still needs credentials. Local endpoints need none,
/// and every hosted model is reached through OpenRouter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    OpenRouter,
}

impl AuthProvider {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
        }
    }

    pub fn env_var(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    fn from_env_var(env_var: &str) -> Option<Self> {
        match env_var {
            "OPENROUTER_API_KEY" => Some(Self::OpenRouter),
            _ => None,
        }
    }
}

/// Serde ignores unknown fields by default, so an older file carrying
/// entries for providers this no longer has still parses; they are dropped
/// rather than failing the load for anyone upgrading.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthFile {
    pub openrouter: Option<ApiKeyEntry>,
}

impl AuthFile {
    fn entry(&self, provider: AuthProvider) -> Option<&ApiKeyEntry> {
        match provider {
            AuthProvider::OpenRouter => self.openrouter.as_ref(),
        }
    }

    fn entry_mut(&mut self, provider: AuthProvider) -> &mut Option<ApiKeyEntry> {
        match provider {
            AuthProvider::OpenRouter => &mut self.openrouter,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    pub r#type: String,
    pub key: String,
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
    /// Key for an endpoint, looked up by the environment variable it names.
    /// An explicitly set env var wins over the stored credential — setting
    /// one is the caller saying "use this", per the usual CLI convention.
    /// Empty env values are ignored.
    pub fn api_key_for(&self, env_var: &str) -> Option<String> {
        env::var(env_var)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .or_else(|| {
                AuthProvider::from_env_var(env_var)
                    .and_then(|provider| self.file.entry(provider))
                    .map(|entry| entry.key.clone())
            })
    }
}

pub fn store_api_key(provider: AuthProvider, key: String) -> Result<PathBuf> {
    let mut auth = load_auth_file().unwrap_or_default();
    *auth.entry_mut(provider) = Some(ApiKeyEntry {
        r#type: "api_key".to_string(),
        key,
    });
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

    #[test]
    fn env_var_beats_stored_key_and_blank_env_is_ignored() {
        let auth = Auth {
            file: AuthFile {
                openrouter: Some(ApiKeyEntry {
                    r#type: "api_key".to_string(),
                    key: "stored".to_string(),
                }),
                ..Default::default()
            },
        };

        // SAFETY: no other test reads or writes this variable concurrently.
        unsafe { env::set_var("OPENROUTER_API_KEY", "from-env") };
        assert_eq!(
            auth.api_key_for(AuthProvider::OpenRouter.env_var())
                .as_deref(),
            Some("from-env")
        );

        // SAFETY: as above.
        unsafe { env::set_var("OPENROUTER_API_KEY", "  ") };
        assert_eq!(
            auth.api_key_for(AuthProvider::OpenRouter.env_var())
                .as_deref(),
            Some("stored")
        );

        // SAFETY: as above.
        unsafe { env::remove_var("OPENROUTER_API_KEY") };
        assert_eq!(
            auth.api_key_for(AuthProvider::OpenRouter.env_var())
                .as_deref(),
            Some("stored")
        );
    }
}
