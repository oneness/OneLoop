use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthFile {
    pub anthropic: Option<ApiKeyEntry>,
    pub openai: Option<ApiKeyEntry>,
    pub zai: Option<ApiKeyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    pub r#type: String,
    pub key: String,
}

pub fn resolve_anthropic_api_key() -> Option<String> {
    load_auth_file()
        .ok()
        .and_then(|auth| auth.anthropic.map(|entry| entry.key))
        .or_else(|| env::var("ANTHROPIC_API_KEY").ok())
}

pub fn resolve_zai_api_key() -> Option<String> {
    load_auth_file()
        .ok()
        .and_then(|auth| auth.zai.map(|entry| entry.key))
        .or_else(|| env::var("ZAI_API_KEY").ok())
}

pub fn resolve_openai_api_key() -> Option<String> {
    load_auth_file()
        .ok()
        .and_then(|auth| auth.openai.map(|entry| entry.key))
        .or_else(|| env::var("OPENAI_API_KEY").ok())
}

pub fn store_anthropic_api_key(key: String) -> Result<PathBuf> {
    let mut auth = load_auth_file().unwrap_or_default();
    auth.anthropic = Some(ApiKeyEntry {
        r#type: "api_key".to_string(),
        key,
    });
    write_auth_file(&auth)
}

pub fn store_zai_api_key(key: String) -> Result<PathBuf> {
    let mut auth = load_auth_file().unwrap_or_default();
    auth.zai = Some(ApiKeyEntry {
        r#type: "api_key".to_string(),
        key,
    });
    write_auth_file(&auth)
}

pub fn store_openai_api_key(key: String) -> Result<PathBuf> {
    let mut auth = load_auth_file().unwrap_or_default();
    auth.openai = Some(ApiKeyEntry {
        r#type: "api_key".to_string(),
        key,
    });
    write_auth_file(&auth)
}

pub fn auth_file_path() -> Result<PathBuf> {
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
    fs::write(&path, json)
        .with_context(|| format!("failed to write auth file: {}", path.display()))?;
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
