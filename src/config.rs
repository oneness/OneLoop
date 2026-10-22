use std::{env, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    /// Populated by `assemble` via `prompt::system_prompt` once the tool
    /// registry exists — the preamble needs the actual tool names.
    pub system_prompt: Option<String>,
    /// Populated by `assemble`: which instruction files this run loaded. A
    /// one-shot leaves it empty.
    pub prompt_sources: Vec<&'static str>,
}

impl Default for Config {
    fn default() -> Self {
        let cwd = env::var("ONELOOP_ORIGINAL_DIR")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())
            .unwrap_or_else(|_| PathBuf::from("."));
        // Both prompt fields are filled by `assemble` once the tool registry
        // exists: which tools registered decides part of both, and a one-shot
        // loads neither.
        Self {
            cwd,
            system_prompt: None,
            prompt_sources: Vec::new(),
        }
    }
}

/// Cap on agent-loop iterations per prompt.
pub const DEFAULT_MAX_ITERATIONS: usize = 100;

/// Attempts one model gets before the user is offered another.
pub const DEFAULT_MAX_RETRIES: usize = 10;

/// Read an env var, falling back to `default` when unset or unparsable.
pub fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_starts_without_prompt_material() {
        let config = Config::default();
        assert!(config.system_prompt.is_none());
        assert!(config.prompt_sources.is_empty());
    }

    #[test]
    fn env_or_falls_back_when_unset() {
        assert_eq!(env_or("ONELOOP_SURELY_UNSET_VAR", 7), 7);
    }
}
