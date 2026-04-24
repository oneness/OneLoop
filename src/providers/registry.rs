use std::env;

use anyhow::{bail, Context, Result};

use super::{AnthropicProvider, MockProvider, OpenAIProvider, Provider, ProviderRequest, ProviderResponse, ZaiProvider};
use crate::auth;

pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
    default_index: usize,
}

impl ProviderRegistry {
    pub fn new() -> Result<Self> {
        let preferred = env::var("ONELOOP_PROVIDER").ok();

        let mut providers: Vec<Box<dyn Provider>> = Vec::new();

        // Always add mock.
        let mock_index = providers.len();
        providers.push(Box::new(MockProvider));

        let mut anthropic_index: Option<usize> = None;
        let mut zai_index: Option<usize> = None;
        let mut openai_index: Option<usize> = None;

        if let Some(key) = auth::resolve_anthropic_api_key() {
            anthropic_index = Some(providers.len());
            providers.push(Box::new(AnthropicProvider::new(key)?));
        }

        if let Some(key) = auth::resolve_zai_api_key() {
            zai_index = Some(providers.len());
            providers.push(Box::new(ZaiProvider::new(key)?));
        }

        if let Some(key) = auth::resolve_openai_api_key() {
            openai_index = Some(providers.len());
            providers.push(Box::new(OpenAIProvider::new(key)?));
        }

        let default_index = match preferred.as_deref() {
            Some("zai") => zai_index.context("ONELOOP_PROVIDER=zai but no ZAI_API_KEY/auth found")?,
            Some("anthropic") => {
                anthropic_index.context("ONELOOP_PROVIDER=anthropic but no ANTHROPIC_API_KEY/auth found")?
            }
            Some("openai") => {
                openai_index.context("ONELOOP_PROVIDER=openai but no OPENAI_API_KEY/auth found")?
            }
            Some("mock") => mock_index,
            Some(other) => bail!("unknown provider: {other}"),
            None => zai_index
                .or(openai_index)
                .or(anthropic_index)
                .unwrap_or(mock_index),
        };

        Ok(Self {
            providers,
            default_index,
        })
    }

    pub fn active_name(&self) -> &'static str {
        self.providers[self.default_index].name()
    }

    pub fn active_model(&self) -> String {
        self.providers[self.default_index].model()
    }

    pub fn available_providers(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn resolve(&self, name: Option<&str>) -> Result<&dyn Provider> {
        let name = name.unwrap_or_else(|| self.providers[self.default_index].name());
        self.providers
            .iter()
            .find(|p| p.name() == name)
            .with_context(|| {
                let available: Vec<&str> = self.providers.iter().map(|p| p.name()).collect();
                format!("unknown provider: {name}. available: {}", available.join(", "))
            })
            .map(|p| p.as_ref())
    }

    pub async fn complete_with(
        &self,
        provider_name: Option<&str>,
        request: ProviderRequest,
    ) -> Result<ProviderResponse> {
        let provider = self.resolve(provider_name)?;
        provider.complete(request).await
    }
}
