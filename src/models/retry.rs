//! What to do when a model does not answer: retry it, then offer another.
//!
//! Beside the registry rather than in `providers` because the cure is a
//! different model, which only the registry knows about.

use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{FallbackRequest, FallbackRequests, ModelRegistry, RetryMode};
use crate::output;
use crate::providers::{ProviderRequest, ProviderResponse, is_retryable};

impl ModelRegistry {
    /// Retries, then offers another model. Returns the alias that answered,
    /// which is not always the one asked for.
    ///
    /// The spinner callbacks let the caller halt its animation before the
    /// prompt appears and resume it once the fallback is chosen. `Silent`
    /// drops the callbacks and the narration: background work cannot borrow a
    /// terminal while the REPL owns it. It still asks the user to pick a
    /// fallback, through `fallbacks` — the REPL runs the picker when it next
    /// reaches a prompt boundary.
    pub async fn complete_with_retry(
        &self,
        alias: Option<&str>,
        request: ProviderRequest,
        stop_spinner: Option<Box<dyn FnOnce() + Send>>,
        start_spinner: Option<Box<dyn FnOnce() + Send>>,
        mode: RetryMode,
        fallbacks: Option<&FallbackRequests>,
    ) -> Result<(String, ProviderResponse)> {
        let max_retries: usize =
            crate::config::env_or("ONELOOP_MAX_RETRIES", crate::config::DEFAULT_MAX_RETRIES);

        let model = self.resolve(alias)?;
        let label = model.alias.as_str();

        let mut last_error: Option<String> = None;
        for attempt in 1..=max_retries {
            match model.complete(request.clone()).await {
                Ok(response) => return Ok((label.to_string(), response)),
                Err(e) => {
                    let err_msg = format!("{e:#}");
                    last_error = Some(err_msg.clone());

                    // Rejected once is rejected identically next time. `context`,
                    // not `bail!`: the caller downcasts to tell a body over the
                    // context limit from any other, and a string loses the type.
                    if !is_retryable(&e) {
                        return Err(e.context(format!("[{label}]")));
                    }

                    if attempt < max_retries {
                        let backoff = Duration::from_millis(500 * attempt as u64);
                        if mode == RetryMode::Verbose {
                            output::warn(&format!(
                                "[{label}] attempt {attempt}/{max_retries} failed: {err_msg}"
                            ));
                            output::note(&format!("⏳ retrying in {}ms...", backoff.as_millis()));
                        }
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        let err_msg = last_error.as_deref().unwrap_or("unknown error");
        let alternatives = self.alternatives(label);

        if mode == RetryMode::Silent {
            // The retries were quiet, but the choice is still the user's.
            // Ask the REPL to put the picker on screen where stdin is free,
            // and wait here for the alias it answers with.
            let Some(requests) = fallbacks else {
                bail!("[{label}] failed after {max_retries} retries: {err_msg}");
            };
            if alternatives.is_empty() {
                bail!(
                    "[{label}] failed after {max_retries} retries and no other model is configured"
                )
            }
            let (reply, chosen) = tokio::sync::oneshot::channel();
            requests.send(FallbackRequest {
                aliases: alternatives,
                reply,
            })?;
            let alias = chosen.await.context("the model choice was not delivered")?;
            let fallback = self.get(&alias)?;
            let response = fallback
                .complete(request)
                .await
                .with_context(|| format!("[{label}] -> {alias}"))?;
            // Like a picked fallback in a turn, it becomes active only after
            // it answers.
            self.set_active(&fallback.alias)?;
            return Ok((fallback.alias.clone(), response));
        }

        output::fail(&format!(
            "[{label}] all {max_retries} attempts failed: {err_msg}"
        ));

        if alternatives.is_empty() {
            bail!("[{label}] failed after {max_retries} retries and no other model is configured")
        }

        // Piped input and scripts have nobody to answer the picker.
        if !io::stdin().is_terminal() {
            bail!("[{label}] failed after {max_retries} retries: {err_msg}");
        }

        if let Some(stop) = stop_spinner {
            stop();
        }

        output::head("Model Unavailable");
        output::note(&format!("{label} is not responding. Pick another:"));
        let fallback = self.get(&self.pick_from(&alternatives).await?)?;

        output::ok(&format!("trying {fallback}"));
        if let Some(start) = start_spinner {
            start();
        }
        let response = fallback.complete(request).await?;

        // A picked model becomes active only after it successfully answers.
        self.set_active(&fallback.alias)?;
        output::ok(&format!("switching to {fallback}"));
        Ok((fallback.alias.clone(), response))
    }

    /// Every alias but the one that failed, in catalog order. Both the turn's
    /// picker and a background choice offer exactly these models.
    fn alternatives(&self, label: &str) -> Vec<String> {
        self.models
            .iter()
            .filter(|model| model.alias != label)
            .map(|model| model.alias.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry every other test in this module uses: two models, no
    /// keyring and no home directory to read.
    fn registry() -> ModelRegistry {
        ModelRegistry::for_test(
            r#"{"default":"main","providers":{"p":{"base_url":"http://u",
                 "api_key_env":"P_KEY","models":{
                   "main":{"id":"main-model"},
                   "backup":{"id":"backup-model"}
                 }}}}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_fallback_menu_never_offers_the_model_that_failed() {
        let registry = registry();

        assert_eq!(registry.alternatives("main"), ["backup"]);
        assert_eq!(registry.alternatives("backup"), ["main"]);
    }
}
