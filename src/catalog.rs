//! `~/.oneloop/config.json`: providers, and the models each one hosts.
//!
//! OpenRouter is one provider serving hundreds of models, so its URL and key
//! are stated once and the models listed under them. Aliases are unique
//! across providers, so a directive never has to name one.
//!
//! No secrets here — a provider names the environment variable its key lives
//! in, which is what keeps the file committable to dotfiles.

use std::collections::BTreeMap;
use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::output::{DIM, RESET};

/// A JSON file rather than structs in Rust: which models exist is
/// configuration, and a value compiled into the binary is one nobody can see
/// or change. Used in memory if the first-run write fails.
const DEFAULT_CONFIG_JSON: &str = include_str!("default-config.json");

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
    /// Server-side web_search/web_fetch, metered per use ($0.005/search,
    /// $0.001/fetch) — off unless a provider asks for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_tools: Option<bool>,
    pub models: BTreeMap<String, ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// What goes on the wire — `~deepseek/deepseek-v4-flash-latest`, not
    /// something to type into a directive, hence the alias.
    pub id: String,
    /// Not a preference: a self-hosted server rejects anything over the `-c`
    /// it was started with, and a hosted model over its own limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_tools: Option<bool>,
}

// ── What this run will use ────────────────────────────────────────────

/// The configuration this run will use.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// The file's `default` unless the environment named another. Guaranteed
    /// to be an alias one of the providers hosts.
    pub active: String,
    pub providers: BTreeMap<String, ProviderEntry>,
}

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
    // ONELOOP_MODEL names the model for this run; ONELOOP_PROVIDER is the
    // name it had before models and providers were separated.
    let requested = env::var("ONELOOP_MODEL")
        .or_else(|_| env::var("ONELOOP_PROVIDER"))
        .ok();
    let mut catalog = resolve(file, requested)?;
    apply_env_overrides(&mut catalog);
    Ok(catalog)
}

/// Settles the active model here, rather than where the models are built,
/// so the per-run overrides below land on the model actually selected.
pub fn resolve(file: ConfigFile, requested: Option<String>) -> Result<Catalog> {
    let catalog = Catalog {
        active: requested.unwrap_or(file.default),
        providers: file.providers,
    };
    catalog.check_aliases_are_unique()?;

    let aliases = catalog.aliases();
    if aliases.is_empty() {
        bail!("no models configured — see ~/.oneloop/config.json");
    }
    if !aliases.contains(&catalog.active.as_str()) {
        bail!(
            "no model named {}. configured: {}",
            catalog.active,
            aliases.join(", ")
        );
    }
    Ok(catalog)
}

impl Catalog {
    fn aliases(&self) -> Vec<&str> {
        self.providers
            .values()
            .flat_map(|provider| provider.models.keys())
            .map(String::as_str)
            .collect()
    }

    /// A duplicate alias would make `#!consensus <alias> ...#!` ambiguous,
    /// and picking one silently is worse than refusing.
    fn check_aliases_are_unique(&self) -> Result<()> {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, provider) in &self.providers {
            for alias in provider.models.keys() {
                if let Some(owner) = seen.insert(alias, name) {
                    bail!(
                        "model alias {alias} is defined by both {owner} and {name}; \
                         aliases must be unique"
                    );
                }
            }
        }
        Ok(())
    }

    fn active_model_mut(&mut self) -> Option<&mut ModelEntry> {
        let active = self.active.clone();
        self.providers
            .values_mut()
            .find_map(|provider| provider.models.get_mut(&active))
    }
}

/// Escape hatches for one-off runs; per-model settings belong in the file.
fn apply_env_overrides(catalog: &mut Catalog) {
    let Some(model) = catalog.active_model_mut() else {
        return;
    };
    if let Some(window) = env::var("ONELOOP_CONTEXT_WINDOW_TOKENS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        model.context_window = Some(window);
    }
    // A cost control, so it stays reachable without editing the file.
    if let Some(web_tools) = env::var("ONELOOP_WEB_TOOLS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        model.web_tools = Some(web_tools);
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        // Covered by a test, so a parse failure is a build-time mistake.
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
        resolve(
            serde_json::from_str(json).expect("test json must parse"),
            None,
        )
    }

    fn template() -> Catalog {
        resolve(ConfigFile::default(), None).expect("template must resolve")
    }

    #[test]
    fn the_shipped_template_resolves() {
        // `Default` unwraps it, so a mistake would panic on first run.
        let catalog = template();
        assert_eq!(catalog.active, "local");
        assert_eq!(catalog.aliases(), vec!["local", "flash"]);
    }

    #[test]
    fn the_default_model_needs_no_key() {
        // An unconfigured checkout must not be able to bill anyone.
        let catalog = template();
        let local = catalog.providers.get("local").unwrap();
        assert!(local.api_key_env.is_none());
        assert!(local.web_tools.is_none());
    }

    #[test]
    fn the_environment_can_pick_another_model() {
        let file = serde_json::from_str(
            r#"{"default":"a","providers":{"p":{"base_url":"u",
                 "models":{"a":{"id":"x"},"b":{"id":"y"}}}}}"#,
        )
        .unwrap();
        let catalog = resolve(file, Some("b".to_string())).unwrap();
        assert_eq!(catalog.active, "b");
    }

    #[test]
    fn a_model_that_is_not_configured_is_refused() {
        // Falling back to something that does exist would send the run — and
        // the spending — somewhere other than what was asked for.
        let json =
            r#"{"default":"a","providers":{"p":{"base_url":"u","models":{"a":{"id":"x"}}}}}"#;
        let from_env = resolve(
            serde_json::from_str(json).unwrap(),
            Some("nope".to_string()),
        );
        assert!(
            format!("{:#}", from_env.unwrap_err()).contains("no model named nope"),
            "the environment naming an unconfigured model must fail"
        );

        let json =
            r#"{"default":"nope","providers":{"p":{"base_url":"u","models":{"a":{"id":"x"}}}}}"#;
        let from_file = parse(json);
        assert!(
            format!("{:#}", from_file.unwrap_err()).contains("no model named nope"),
            "the file naming an unconfigured model must fail"
        );
    }

    #[test]
    fn a_duplicate_alias_across_providers_is_refused() {
        // Picking one would make `#!consensus dup ...#!` mean whichever
        // provider happened to sort first.
        let err = parse(
            r#"{"default":"dup","providers":{
                 "p1":{"base_url":"u1","models":{"dup":{"id":"x"}}},
                 "p2":{"base_url":"u2","models":{"dup":{"id":"y"}}}}}"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unique"), "got: {err:#}");
    }

    #[test]
    fn a_catalog_with_no_models_is_refused() {
        let err = parse(r#"{"default":"a","providers":{}}"#).unwrap_err();
        assert!(format!("{err:#}").contains("no models"), "got: {err:#}");
    }
}
