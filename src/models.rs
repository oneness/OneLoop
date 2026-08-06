//! The models this run can reach, and which one is active.
//!
//! `catalog` says what is configured; this makes it live. Models of one
//! provider share an `Arc` of it, so they share its connection pool and key.
//! The list is flat because every lookup here is by alias.

use std::fmt;
use std::io::{self, Write as IoWrite};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};

use crate::auth;
use crate::catalog::{self, Catalog};
use crate::output::{BOLD, DIM, RESET};
use crate::providers::{Provider, ProviderRequest, ProviderResponse, chat};

mod retry;

/// Used when a model does not state its own.
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

/// One model, ready to answer.
#[derive(Debug)]
pub struct Model {
    /// The short name used everywhere else — directives, `/model`, `default`.
    pub alias: String,
    /// What goes on the wire.
    pub id: String,
    pub provider: Arc<Provider>,
    /// Not a tuning knob: too high and the request is rejected outright,
    /// with nothing to fall back on.
    pub context_window: usize,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub web_tools: bool,
}

impl Model {
    pub async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        chat::complete(self, request).await
    }
}

impl fmt::Display for Model {
    /// `flash · openrouter (~deepseek/deepseek-v4-flash-latest)`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} · {} ({})", self.alias, self.provider.name, self.id)
    }
}

pub struct ModelRegistry {
    models: Vec<Model>,
    /// Atomic because `/model` can switch it while orchestration tasks
    /// already hold the registry behind an `Arc`.
    active: AtomicUsize,
}

impl ModelRegistry {
    pub fn new() -> Result<Self> {
        let catalog = catalog::load()?;
        let auth = auth::load();
        Self::build(&catalog, |var| auth.api_key_for(var))
    }

    /// Split from `new` so tests can build one without a home directory or
    /// a keyring.
    fn build(catalog: &Catalog, api_key: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut models = Vec::new();
        for (name, entry) in &catalog.providers {
            // Built with or without a key: a local server needs none.
            let key = entry.api_key_env.as_deref().and_then(&api_key);
            let provider = Arc::new(Provider::new(name, &entry.base_url, key)?);
            models.extend(entry.models.iter().map(|(alias, model)| Model {
                alias: alias.clone(),
                id: model.id.clone(),
                provider: Arc::clone(&provider),
                context_window: model.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
                max_tokens: model.max_tokens,
                temperature: model.temperature,
                web_tools: model.web_tools.or(entry.web_tools).unwrap_or(false),
            }));
        }

        // The catalog guarantees this alias exists; `position` rather than
        // an index because a promise is not a bounds check.
        let active = models
            .iter()
            .position(|m| m.alias == catalog.active)
            .with_context(|| format!("catalog named an unknown model: {}", catalog.active))?;

        Ok(Self {
            models,
            active: AtomicUsize::new(active),
        })
    }

    /// The model requests use when none names another.
    pub fn active(&self) -> &Model {
        &self.models[self.active.load(Ordering::Relaxed)]
    }

    /// This run only: the config file is never written, so the next run
    /// starts where its `default` says. An unknown alias changes nothing.
    pub fn set_active(&self, alias: &str) -> Result<()> {
        let index = self
            .models
            .iter()
            .position(|m| m.alias == alias)
            .with_context(|| self.unknown_alias(alias))?;
        self.active.store(index, Ordering::Relaxed);
        Ok(())
    }

    pub fn get(&self, alias: &str) -> Result<&Model> {
        self.models
            .iter()
            .find(|m| m.alias == alias)
            .with_context(|| self.unknown_alias(alias))
    }

    /// The model named, or the active one.
    pub fn resolve(&self, alias: Option<&str>) -> Result<&Model> {
        match alias {
            Some(alias) => self.get(alias),
            None => Ok(self.active()),
        }
    }

    pub fn aliases(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.alias.as_str()).collect()
    }

    /// `local (local), openrouter (flash, sonnet)`.
    pub fn by_provider(&self) -> String {
        // Built provider by provider, so equal names are already adjacent.
        let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
        for model in &self.models {
            match groups.last_mut() {
                Some((provider, aliases)) if *provider == model.provider.name => {
                    aliases.push(&model.alias)
                }
                _ => groups.push((&model.provider.name, vec![&model.alias])),
            }
        }
        groups
            .iter()
            .map(|(provider, aliases)| format!("{provider} ({})", aliases.join(", ")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Compaction needs this before the request exists, so it resolves by
    /// alias rather than reading a global.
    pub fn context_window_for(&self, alias: Option<&str>) -> usize {
        self.resolve(alias)
            .unwrap_or_else(|_| self.active())
            .context_window
    }

    fn unknown_alias(&self, alias: &str) -> String {
        // Grouped, because the next question is which provider had it.
        format!("unknown model: {alias}. available: {}", self.by_provider())
    }

    pub async fn complete_once(
        &self,
        alias: &str,
        request: ProviderRequest,
    ) -> Result<ProviderResponse> {
        self.get(alias)?.complete(request).await
    }

    /// Offer every configured model — what `/model` asks for.
    pub async fn pick_any(&self) -> Result<String> {
        let all: Vec<&Model> = self.models.iter().collect();
        self.pick(&all).await
    }

    /// A numbered menu grouped by provider. Returns the chosen alias;
    /// cancelling is an error, so no caller mistakes it for a choice.
    async fn pick(&self, candidates: &[&Model]) -> Result<String> {
        let active = self.active().alias.as_str();
        let width = candidates
            .iter()
            .map(|model| model.alias.len())
            .max()
            .unwrap_or(0);
        let mut listed_provider = "";
        for (i, model) in candidates.iter().enumerate() {
            if model.provider.name != listed_provider {
                listed_provider = &model.provider.name;
                println!("{DIM}  {listed_provider}{RESET}");
            }
            let marker = if model.alias == active {
                "  ← active"
            } else {
                ""
            };
            println!(
                "{BOLD}    {}. {:width$}  {DIM}{}{marker}{RESET}",
                i + 1,
                model.alias,
                model.id
            );
        }
        let index = read_choice(candidates.len()).await?;
        let Some(model) = candidates.get(index) else {
            bail!("invalid selection");
        };
        Ok(model.alias.clone())
    }
}

/// Zero-based index of the chosen line.
async fn read_choice(count: usize) -> Result<usize> {
    print!("{BOLD}  → select [1-{count}] or Enter to cancel: {RESET}");
    io::stdout().flush()?;

    // On a blocking thread, or Ctrl+C could not be heard until after the
    // answer it is trying to avoid.
    let choice = tokio::select! {
        res = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            io::stdin().read_line(&mut buf).map(|_| buf)
        }) => res,
        _ = tokio::signal::ctrl_c() => {
            println!();
            bail!("cancelled — no model selected");
        }
    };

    let choice = match choice {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => bail!("failed to read input: {e}"),
        Err(e) => bail!("input thread failed: {e}"),
    };

    let trimmed = choice.trim();
    if trimmed.is_empty() {
        bail!("cancelled — no model selected");
    }
    trimmed
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .filter(|index| *index < count)
        .with_context(|| format!("invalid selection: {trimmed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(json: &str) -> ModelRegistry {
        let file = serde_json::from_str(json).expect("test json must parse");
        let catalog = catalog::resolve(file, None).expect("catalog must resolve");
        ModelRegistry::build(&catalog, |var| Some(format!("key-for-{var}")))
            .expect("registry must build")
    }

    #[test]
    fn models_of_one_provider_share_its_endpoint() {
        let registry = registry(
            r#"{"default":"a","providers":{"p":{"base_url":"http://u",
                 "models":{"a":{"id":"vendor/a"},"b":{"id":"vendor/b"}}}}}"#,
        );
        let [a, b] = registry.models.as_slice() else {
            panic!("expected two models");
        };
        assert!(Arc::ptr_eq(&a.provider, &b.provider));
        assert_eq!(a.id, "vendor/a");
        assert_eq!(b.id, "vendor/b");
        assert_eq!(a.provider.name, "p");
    }

    #[test]
    fn a_model_overrides_its_provider_and_falls_back_when_it_says_nothing() {
        let registry = registry(
            r#"{"default":"a","providers":{"p":{
                 "base_url":"http://u","web_tools":true,
                 "models":{"a":{"id":"x","web_tools":false,"context_window":9},
                           "b":{"id":"y"}}}}}"#,
        );
        let a = registry.get("a").unwrap();
        assert!(!a.web_tools, "model must win over provider");
        assert_eq!(a.context_window, 9);

        let b = registry.get("b").unwrap();
        assert!(b.web_tools, "provider otherwise");
        assert_eq!(b.context_window, DEFAULT_CONTEXT_WINDOW, "then the default");
    }

    #[test]
    fn the_active_model_can_be_switched() {
        let registry = registry(
            r#"{"default":"a","providers":{"p":{"base_url":"u",
                 "models":{"a":{"id":"x"},"b":{"id":"y"}}}}}"#,
        );
        assert_eq!(registry.active().alias, "a");
        registry.set_active("b").unwrap();
        assert_eq!(registry.active().alias, "b");
    }

    #[test]
    fn switching_to_an_unconfigured_model_changes_nothing() {
        let registry = registry(
            r#"{"default":"a","providers":{"p":{"base_url":"u","models":{"a":{"id":"x"}}}}}"#,
        );
        let err = registry.set_active("nope").unwrap_err();
        assert!(format!("{err:#}").contains("unknown model"), "got: {err:#}");
        assert_eq!(registry.active().alias, "a");
    }
}
