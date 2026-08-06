use anyhow::{Result, bail};

const MAX_ROUNDS: usize = 3;

// ── Public types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDirectives {
    pub mode: RunMode,
    pub judge: Option<String>,
    pub rounds: usize,
    pub tools: ToolMode,
    pub format: OutputFormat,
    /// `model:` — a wire id to use in place of the selected model's own,
    /// for this prompt only. Not an alias: it names something the provider
    /// hosts that the config never listed.
    pub model_id: Option<String>,
    pub prompt: String,
}

/// Model names here are aliases from the catalog, never wire ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    Single { model: Option<String> },
    Consensus { models: Vec<String> },
    Debate { models: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolMode {
    Default,
    None,
    AllowList(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputFormat {
    Plain,
    Md,
    Html,
}

// ── Parser ────────────────────────────────────────────────────────────

/// Parse user input into a `PromptDirectives`.
///
/// Syntax: `#!<directive words>#! <user message>`
///
/// - No `#!` at all → default single mode, full input is the body.
/// - `#!...#!` → directive tokens between the markers, body after closing `#!`.
pub fn parse_prompt(input: &str, known_models: &[&str]) -> Result<PromptDirectives> {
    let trimmed = input.trim();

    // No directive marker → plain prompt with default single mode.
    if !trimmed.starts_with("#!") {
        return Ok(PromptDirectives {
            mode: RunMode::Single { model: None },
            judge: None,
            rounds: 1,
            tools: ToolMode::Default,
            format: OutputFormat::Plain,
            model_id: None,
            prompt: trimmed.to_string(),
        });
    }

    // Find the closing #!.
    let after_open = &trimmed[2..]; // skip opening "#!"
    let Some(close_pos) = after_open.find("#!") else {
        bail!("directive missing closing #! — use: #!<directive words>#! <your message>");
    };

    let directive_text = after_open[..close_pos].trim();
    let body = after_open[close_pos + 2..].trim().to_string();

    if directive_text.is_empty() {
        bail!("directive between #! ... #! is empty");
    }
    if body.is_empty() {
        bail!("prompt body after #! is empty");
    }

    let tokens: Vec<&str> = directive_text.split_whitespace().collect();

    // Collect tokens into categories.
    let mut models: Vec<String> = Vec::new();
    let mut mode_name: Option<&str> = None;
    let mut judge: Option<String> = None;
    let mut rounds: Option<usize> = None;
    let mut tools: Option<ToolMode> = None;
    let mut format: Option<OutputFormat> = None;
    let mut model_id: Option<String> = None;

    for token in &tokens {
        // key:value pairs
        if let Some(kv) = token.strip_prefix("model:") {
            if model_id.is_some() {
                bail!("duplicate model: directive");
            }
            let id = kv.trim().to_string();
            if id.is_empty() {
                bail!("model: requires a model id");
            }
            model_id = Some(id);
        } else if let Some(kv) = token.strip_prefix("judge:") {
            if judge.is_some() {
                bail!("duplicate judge: directive");
            }
            let alias = kv.trim().to_string();
            if alias.is_empty() {
                bail!("judge: requires a model name");
            }
            judge = Some(alias);
        } else if let Some(kv) = token.strip_prefix("rounds:") {
            if rounds.is_some() {
                bail!("duplicate rounds: directive");
            }
            let r: usize = kv
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("rounds: must be a positive integer"))?;
            if r == 0 || r > MAX_ROUNDS {
                bail!("rounds: must be between 1 and {MAX_ROUNDS}");
            }
            rounds = Some(r);
        } else if let Some(kv) = token.strip_prefix("tools:") {
            if tools.is_some() {
                bail!("duplicate tools: directive");
            }
            let val = kv.trim();
            if val == "none" {
                tools = Some(ToolMode::None);
            } else {
                let names: Vec<String> = val
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if names.is_empty() {
                    bail!("tools: requires at least one tool name");
                }
                tools = Some(ToolMode::AllowList(names));
            }
        } else if let Some(kv) = token.strip_prefix("format:") {
            if format.is_some() {
                bail!("duplicate format: directive");
            }
            let val = kv.trim();
            format = Some(match val {
                "md" | "markdown" => OutputFormat::Md,
                "html" => OutputFormat::Html,
                other => bail!("unknown format: {other} (supported: md, html)"),
            });
        }
        // Mode keywords
        else if *token == "consensus" || *token == "debate" {
            if mode_name.is_some() {
                bail!("only one mode (consensus or debate) allowed");
            }
            mode_name = Some(token);
        }
        // Model aliases
        else if known_models.contains(token) {
            models.push(token.to_string());
        } else {
            bail!(
                "unknown directive token: {token} (models: {})",
                known_models.join(", ")
            );
        }
    }

    // Resolve mode.
    let mode = resolve_mode(mode_name, models)?;

    // Cross-validate.
    let is_multi = matches!(&mode, RunMode::Consensus { .. } | RunMode::Debate { .. });
    let is_debate = matches!(&mode, RunMode::Debate { .. });

    if judge.is_some() && !is_multi {
        bail!("judge: is only valid with consensus or debate mode");
    }
    if rounds.is_some() && !is_debate {
        bail!("rounds: is only valid with debate mode");
    }
    if tools.is_some() && !is_multi {
        bail!("tools: is only valid with consensus or debate mode");
    }
    if model_id.is_some() && is_multi {
        bail!("model: is only valid in single-model mode");
    }

    Ok(PromptDirectives {
        mode,
        judge,
        rounds: rounds.unwrap_or(1),
        tools: tools.unwrap_or(ToolMode::Default),
        format: format.unwrap_or(OutputFormat::Plain),
        model_id,
        prompt: body,
    })
}

fn resolve_mode(mode_name: Option<&str>, models: Vec<String>) -> Result<RunMode> {
    match (mode_name, models.len()) {
        // Explicit consensus with models.
        (Some("consensus"), n) if n >= 2 => Ok(RunMode::Consensus { models }),
        (Some("consensus"), _) => bail!("consensus requires at least two model names"),

        // Explicit debate with models.
        (Some("debate"), n) if n >= 2 => Ok(RunMode::Debate { models }),
        (Some("debate"), _) => bail!("debate requires at least two model names"),

        // No explicit mode, multiple models → default to consensus.
        (None, n) if n >= 2 => Ok(RunMode::Consensus { models }),

        // No explicit mode, one model → single mode.
        (None, 1) => Ok(RunMode::Single {
            model: models.into_iter().next(),
        }),

        // No mode, no models → just a plain prompt (single mode, active model).
        (None, 0) => Ok(RunMode::Single { model: None }),

        // Mode with no models is nonsensical — but shouldn't reach here.
        _ => bail!("invalid directive combination"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{OutputFormat, PromptDirectives, RunMode, ToolMode, parse_prompt};

    /// Model aliases for the parser under test. Real ones come from the
    /// catalog; these stand in for it.
    const MODELS: &[&str] = &["local", "openrouter", "sonnet"];

    #[test]
    fn plain_prompt_uses_default_single_mode() {
        let got = parsed("hello");
        assert_eq!(got.mode, RunMode::Single { model: None });
        assert_eq!(got.prompt, "hello");
    }

    #[test]
    fn single_model_shorthand() {
        let got = parsed("#!local#! explain this");
        assert_eq!(
            got.mode,
            RunMode::Single {
                model: Some("local".to_string())
            }
        );
        assert_eq!(got.prompt, "explain this");
    }

    #[test]
    fn multi_model_defaults_to_consensus() {
        let got = parsed("#!local openrouter#! should we do this");
        assert_eq!(
            got.mode,
            RunMode::Consensus {
                models: vec!["local".to_string(), "openrouter".to_string()]
            }
        );
        assert_eq!(got.prompt, "should we do this");
    }

    #[test]
    fn explicit_consensus_with_judge() {
        let got = parsed("#!consensus local openrouter judge:openrouter#! hello");
        assert_eq!(
            got.mode,
            RunMode::Consensus {
                models: vec!["local".to_string(), "openrouter".to_string()]
            }
        );
        assert_eq!(got.judge, Some("openrouter".to_string()));
    }

    #[test]
    fn debate_with_rounds_and_judge() {
        let got = parsed("#!debate local openrouter openrouter rounds:2 judge:local#! hello");
        assert_eq!(
            got.mode,
            RunMode::Debate {
                models: vec![
                    "local".to_string(),
                    "openrouter".to_string(),
                    "openrouter".to_string()
                ]
            }
        );
        assert_eq!(got.rounds, 2);
        assert_eq!(got.judge, Some("local".to_string()));
    }

    #[test]
    fn tools_none() {
        let got = parsed("#!consensus local openrouter tools:none#! hello");
        assert_eq!(got.tools, ToolMode::None);
    }

    #[test]
    fn tools_allow_list() {
        let got = parsed("#!consensus local openrouter tools:read,shell#! hello");
        assert_eq!(
            got.tools,
            ToolMode::AllowList(vec!["read".to_string(), "shell".to_string()])
        );
    }

    #[test]
    fn format_md() {
        let got = parsed("#!local format:md#! summarize this");
        assert_eq!(got.format, OutputFormat::Md);
    }

    #[test]
    fn format_html() {
        let got = parsed("#!local format:html#! summarize this");
        assert_eq!(got.format, OutputFormat::Html);
    }

    #[test]
    fn incompatible_modes_fail() {
        let got = parse_prompt("#!consensus debate local openrouter#! hello", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn judge_on_a_single_model_fails() {
        let got = parse_prompt("#!local judge:openrouter#! hello", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn rounds_on_consensus_fails() {
        let got = parse_prompt("#!consensus local openrouter rounds:2#! hello", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn missing_close_marker_fails() {
        let got = parse_prompt("#!local hello", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn empty_directive_fails() {
        let got = parse_prompt("#!#!#! hello", MODELS);
        // "!" between markers is an unknown token
        assert!(got.is_err());
    }

    #[test]
    fn empty_body_fails() {
        let got = parse_prompt("#!local#!", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn no_models_no_mode_is_plain() {
        let got = parsed("#!format:md#! summarize this file");
        assert_eq!(got.mode, RunMode::Single { model: None });
        assert_eq!(got.format, OutputFormat::Md);
    }

    #[test]
    fn model_id_override_with_an_alias() {
        let got = parsed("#!openrouter model:deepseek/deepseek-v3-0324#! explain this");
        assert_eq!(
            got.mode,
            RunMode::Single {
                model: Some("openrouter".to_string())
            }
        );
        assert_eq!(got.model_id, Some("deepseek/deepseek-v3-0324".to_string()));
        assert_eq!(got.prompt, "explain this");
    }

    #[test]
    fn model_id_override_without_an_alias() {
        let got = parsed("#!model:deepseek/deepseek-v3-0324#! explain this");
        assert_eq!(got.mode, RunMode::Single { model: None });
        assert_eq!(got.model_id, Some("deepseek/deepseek-v3-0324".to_string()));
    }

    #[test]
    fn model_id_override_in_consensus_fails() {
        let got = parse_prompt("#!consensus local openrouter model:gpt-4o#! hello", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn tools_allowlist_double_comma_filters_empty() {
        // Double comma should not produce an empty-string entry — it's silently
        // collapsed, giving the same result as a single comma.
        let got = parsed("#!consensus local openrouter tools:read,,bash#! hello");
        assert_eq!(
            got.tools,
            ToolMode::AllowList(vec!["read".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn tools_allowlist_only_commas_errors() {
        // A value of only commas produces no valid names after filtering.
        let got = parse_prompt("#!tools:,#! hello", MODELS);
        assert!(got.is_err());
    }

    #[test]
    fn tools_on_a_single_model_fails() {
        // Only consensus/debate orchestration consumes tools: — reject it
        // elsewhere instead of silently ignoring it.
        let got = parse_prompt("#!local tools:none#! hello", MODELS);
        assert!(got.is_err());
    }

    fn parsed(input: &str) -> PromptDirectives {
        match parse_prompt(input, MODELS) {
            Ok(value) => value,
            Err(e) => panic!("failed to parse prompt: {e:#}"),
        }
    }
}
