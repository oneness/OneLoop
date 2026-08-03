//! What OneLoop can talk to: providers, and the models each one hosts.
//!
//! A provider is a place — a base URL and, if hosted, the environment
//! variable holding its key. A model is one thing that place will run.
//! OpenRouter is a single provider serving hundreds of models, so the URL
//! and key are stated once and the models listed under them; a local
//! llama-server is a provider that happens to host one.
//!
//! Every model carries a short alias, which is the name used everywhere
//! else — `#!consensus flash sonnet#!` rather than the wire ids those
//! resolve to. Aliases are unique across all providers, so a directive
//! never has to say which provider it meant.
//!
//! Config is `~/.oneloop/config.json`, written from a template on first
//! run. It holds no secrets: a model names the environment variable its key
//! lives in, and the key itself is `auth.json`'s business. That keeps the
//! config shareable — committable to dotfiles, diffable, pasteable — which
//! it could not be if a key were in it.

use std::collections::BTreeMap;
use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::output::{DIM, RESET};

/// Written to `~/.oneloop/config.json` the first time OneLoop runs without
/// one, and used in memory if that write fails. A JSON file rather than
/// structs built in Rust: which models exist is configuration, and a value
/// compiled into the binary is one nobody can see or change.
const DEFAULT_CONFIG_JSON: &str = include_str!("default-config.json");

/// Used when a model does not state its own.
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

// ── On-disk shape ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    /// Alias of the model used when nothing else is asked for.
    pub default: String,
    pub providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub base_url: String,
    /// Absent means no credentials — the local server case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Server-side web_search/web_fetch. Metered per use ($0.005/search,
    /// $0.001/fetch) and absent from a local server, so it is off unless a
    /// provider asks for it. A model may override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_tools: Option<bool>,
    pub models: BTreeMap<String, ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// What goes on the wire. Kept separate from the alias because the real
    /// ids are unwieldy — `~deepseek/deepseek-v4-flash-latest` is not
    /// something to type into a directive.
    pub id: String,
    /// Tokens this model accepts in one request; what compaction aims to
    /// stay under. Not a preference — a self-hosted server rejects anything
    /// over the `-c` it was started with, and a hosted model rejects
    /// anything over its own limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_tools: Option<bool>,
}

// ── Resolved shape ────────────────────────────────────────────────────

/// One model with its provider's settings folded in. Everything downstream
/// works on these, so nothing else has to know about the nesting.
#[derive(Debug, Clone)]
pub struct Model {
    pub alias: String,
    pub provider: String,
    pub base_url: String,
    pub id: String,
    pub api_key_env: Option<String>,
    pub web_tools: bool,
    pub context_window: usize,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    /// Alias of the default model; guaranteed to be present in `models`.
    pub default: String,
    pub models: Vec<Model>,
}

/// Load the catalog, applying environment overrides to the default model.
///
/// A missing file is written from the template. A malformed one is an error
/// rather than a silent fallback: a typo should not quietly send work
/// somewhere other than where it says.
pub fn load() -> Result<Catalog> {
    let file = match read_file()? {
        Some(file) => file,
        None => {
            match write_default_file() {
                Ok(path) => eprintln!("{DIM}  → wrote starter config: {}{RESET}", path.display()),
                Err(e) => eprintln!(
                    "{DIM}  → could not write config ({e:#}); using built-in defaults{RESET}"
                ),
            }
            ConfigFile::default()
        }
    };
    let mut catalog = resolve(file)?;
    apply_env_overrides(&mut catalog);
    Ok(catalog)
}

/// Flatten providers into models, folding provider settings into each and
/// rejecting anything a directive could not name unambiguously.
pub fn resolve(file: ConfigFile) -> Result<Catalog> {
    let mut models: Vec<Model> = Vec::new();
    for (provider_name, provider) in &file.providers {
        for (alias, model) in &provider.models {
            // Two providers offering the same alias would make
            // `#!consensus <alias> ...#!` ambiguous, and picking one
            // silently is worse than refusing.
            if let Some(existing) = models.iter().find(|m| &m.alias == alias) {
                bail!(
                    "model alias {alias} is defined by both {} and {provider_name}; \
                     aliases must be unique",
                    existing.provider
                );
            }
            models.push(Model {
                alias: alias.clone(),
                provider: provider_name.clone(),
                base_url: provider.base_url.clone(),
                id: model.id.clone(),
                api_key_env: provider.api_key_env.clone(),
                web_tools: model.web_tools.or(provider.web_tools).unwrap_or(false),
                context_window: model.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
                max_tokens: model.max_tokens,
                temperature: model.temperature,
            });
        }
    }

    if models.is_empty() {
        bail!("no models configured — see ~/.oneloop/config.json");
    }
    if !models.iter().any(|m| m.alias == file.default) {
        let available: Vec<&str> = models.iter().map(|m| m.alias.as_str()).collect();
        bail!(
            "default model {} is not defined. available: {}",
            file.default,
            available.join(", ")
        );
    }

    Ok(Catalog {
        default: file.default,
        models,
    })
}

/// Escape hatches that apply to the default model only. Per-model settings
/// belong in the config file; these exist for one-off runs.
fn apply_env_overrides(catalog: &mut Catalog) {
    let default_alias = catalog.default.clone();
    let Some(model) = catalog.models.iter_mut().find(|m| m.alias == default_alias) else {
        return;
    };
    if let Some(window) = env::var("ONELOOP_CONTEXT_WINDOW_TOKENS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        model.context_window = window;
    }
    // A cost control, so it stays reachable without editing the file.
    if let Some(web_tools) = env::var("ONELOOP_WEB_TOOLS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        model.web_tools = web_tools;
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        // The template ships with the binary and is covered by a test, so a
        // parse failure is a build-time mistake, not a runtime one.
        serde_json::from_str(DEFAULT_CONFIG_JSON).expect("built-in default-config.json must parse")
    }
}

fn config_file_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".oneloop").join("config.json"))
}

fn write_default_file() -> Result<PathBuf> {
    let path = config_file_path()?;
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    fs::write(&path, DEFAULT_CONFIG_JSON)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn read_file() -> Result<Option<ConfigFile>> {
    let path = config_file_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;
    let file = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config: {}", path.display()))?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Catalog> {
        resolve(serde_json::from_str(json).expect("test json must parse"))
    }

    #[test]
    fn the_shipped_template_resolves() {
        // `Default` unwraps the template, so a mistake in it would panic on
        // first run rather than fail the build.
        let catalog = load_template();
        assert_eq!(catalog.default, "local");
        assert!(catalog.models.iter().any(|m| m.alias == "local"));
        assert!(catalog.models.iter().any(|m| m.alias == "flash"));
    }

    fn load_template() -> Catalog {
        resolve(ConfigFile::default()).expect("template must resolve")
    }

    #[test]
    fn the_default_model_needs_no_key() {
        // An unconfigured checkout must not be able to bill anyone.
        let catalog = load_template();
        let default = catalog
            .models
            .iter()
            .find(|m| m.alias == catalog.default)
            .unwrap();
        assert!(default.api_key_env.is_none());
        assert!(!default.web_tools);
    }

    #[test]
    fn provider_settings_fold_into_each_model() {
        let catalog = parse(
            r#"{"default":"a","providers":{"p":{
                 "base_url":"http://u","api_key_env":"K","web_tools":true,
                 "models":{"a":{"id":"vendor/a"},"b":{"id":"vendor/b"}}}}}"#,
        )
        .unwrap();
        for m in &catalog.models {
            assert_eq!(m.base_url, "http://u", "url is stated once, on the provider");
            assert_eq!(m.api_key_env.as_deref(), Some("K"));
            assert!(m.web_tools);
        }
        assert_eq!(catalog.models.iter().find(|m| m.alias == "a").unwrap().id, "vendor/a");
    }

    #[test]
    fn a_model_overrides_its_provider() {
        let catalog = parse(
            r#"{"default":"a","providers":{"p":{
                 "base_url":"http://u","web_tools":true,
                 "models":{"a":{"id":"x","web_tools":false,"context_window":9}}}}}"#,
        )
        .unwrap();
        let a = &catalog.models[0];
        assert!(!a.web_tools, "model must win over provider");
        assert_eq!(a.context_window, 9);
    }

    #[test]
    fn an_unset_context_window_falls_back() {
        let catalog =
            parse(r#"{"default":"a","providers":{"p":{"base_url":"u","models":{"a":{"id":"x"}}}}}"#)
                .unwrap();
        assert_eq!(catalog.models[0].context_window, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn a_duplicate_alias_across_providers_is_refused() {
        // Silently picking one would make `#!consensus dup ...#!` mean
        // whichever provider happened to sort first.
        let err = parse(
            r#"{"default":"dup","providers":{
                 "p1":{"base_url":"u1","models":{"dup":{"id":"x"}}},
                 "p2":{"base_url":"u2","models":{"dup":{"id":"y"}}}}}"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unique"), "got: {err:#}");
    }

    #[test]
    fn a_default_naming_no_model_is_refused() {
        let err =
            parse(r#"{"default":"nope","providers":{"p":{"base_url":"u","models":{"a":{"id":"x"}}}}}"#)
                .unwrap_err();
        assert!(format!("{err:#}").contains("not defined"), "got: {err:#}");
    }

    #[test]
    fn a_catalog_with_no_models_is_refused() {
        let err = parse(r#"{"default":"a","providers":{}}"#).unwrap_err();
        assert!(format!("{err:#}").contains("no models"), "got: {err:#}");
    }

    #[test]
    fn the_file_round_trips() {
        let file = ConfigFile::default();
        let json = serde_json::to_string(&file).unwrap();
        let back: ConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default, file.default);
        assert_eq!(back.providers.len(), file.providers.len());
    }
}
