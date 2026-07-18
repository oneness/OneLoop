use anyhow::{Result, bail};

const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openrouter"];
const MAX_ROUNDS: usize = 3;

// ── Public types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDirectives {
    pub mode: RunMode,
    pub judge: Option<String>,
    pub rounds: usize,
    pub tools: ToolMode,
    pub format: OutputFormat,
    pub model: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    Single { provider: Option<String> },
    Consensus { providers: Vec<String> },
    Debate { providers: Vec<String> },
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
pub fn parse_prompt(input: &str) -> Result<PromptDirectives> {
    let trimmed = input.trim();

    // No directive marker → plain prompt with default single mode.
    if !trimmed.starts_with("#!") {
        return Ok(PromptDirectives {
            mode: RunMode::Single { provider: None },
            judge: None,
            rounds: 1,
            tools: ToolMode::Default,
            format: OutputFormat::Plain,
            model: None,
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
    let mut providers: Vec<String> = Vec::new();
    let mut mode_name: Option<&str> = None;
    let mut judge: Option<String> = None;
    let mut rounds: Option<usize> = None;
    let mut tools: Option<ToolMode> = None;
    let mut format: Option<OutputFormat> = None;
    let mut model: Option<String> = None;

    for token in &tokens {
        // key:value pairs
        if let Some(kv) = token.strip_prefix("model:") {
            if model.is_some() {
                bail!("duplicate model: directive");
            }
            let m = kv.trim().to_string();
            if m.is_empty() {
                bail!("model: requires a model name");
            }
            model = Some(m);
        } else if let Some(kv) = token.strip_prefix("judge:") {
            if judge.is_some() {
                bail!("duplicate judge: directive");
            }
            let provider = kv.trim().to_string();
            if provider.is_empty() {
                bail!("judge: requires a provider name");
            }
            judge = Some(provider);
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
        // Provider names
        else if is_known_provider(token) {
            providers.push(token.to_string());
        } else {
            bail!("unknown directive token: {token}");
        }
    }

    // Resolve mode.
    let mode = resolve_mode(mode_name, providers)?;

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
    if model.is_some() && is_multi {
        bail!("model: is only valid in single-provider mode");
    }

    Ok(PromptDirectives {
        mode,
        judge,
        rounds: rounds.unwrap_or(1),
        tools: tools.unwrap_or(ToolMode::Default),
        format: format.unwrap_or(OutputFormat::Plain),
        model,
        prompt: body,
    })
}

fn resolve_mode(mode_name: Option<&str>, providers: Vec<String>) -> Result<RunMode> {
    match (mode_name, providers.len()) {
        // Explicit consensus with providers.
        (Some("consensus"), n) if n >= 2 => Ok(RunMode::Consensus { providers }),
        (Some("consensus"), _) => bail!("consensus requires at least two provider names"),

        // Explicit debate with providers.
        (Some("debate"), n) if n >= 2 => Ok(RunMode::Debate { providers }),
        (Some("debate"), _) => bail!("debate requires at least two provider names"),

        // No explicit mode, multiple providers → default to consensus.
        (None, n) if n >= 2 => Ok(RunMode::Consensus { providers }),

        // No explicit mode, single provider → single mode.
        (None, 1) => Ok(RunMode::Single {
            provider: providers.into_iter().next(),
        }),

        // No mode, no providers → just a plain prompt (single mode, no provider override).
        (None, 0) => Ok(RunMode::Single { provider: None }),

        // Mode with no providers is nonsensical — but shouldn't reach here.
        _ => bail!("invalid directive combination"),
    }
}

fn is_known_provider(value: &str) -> bool {
    KNOWN_PROVIDERS.contains(&value)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{OutputFormat, PromptDirectives, RunMode, ToolMode, parse_prompt};

    #[test]
    fn plain_prompt_uses_default_single_mode() {
        let got = parsed("hello");
        assert_eq!(got.mode, RunMode::Single { provider: None });
        assert_eq!(got.prompt, "hello");
    }

    #[test]
    fn single_provider_shorthand() {
        let got = parsed("#!anthropic#! explain this");
        assert_eq!(
            got.mode,
            RunMode::Single {
                provider: Some("anthropic".to_string())
            }
        );
        assert_eq!(got.prompt, "explain this");
    }

    #[test]
    fn multi_provider_defaults_to_consensus() {
        let got = parsed("#!anthropic openai#! should we do this");
        assert_eq!(
            got.mode,
            RunMode::Consensus {
                providers: vec!["anthropic".to_string(), "openai".to_string()]
            }
        );
        assert_eq!(got.prompt, "should we do this");
    }

    #[test]
    fn explicit_consensus_with_judge() {
        let got = parsed("#!consensus anthropic openai judge:openai#! hello");
        assert_eq!(
            got.mode,
            RunMode::Consensus {
                providers: vec!["anthropic".to_string(), "openai".to_string()]
            }
        );
        assert_eq!(got.judge, Some("openai".to_string()));
    }

    #[test]
    fn debate_with_rounds_and_judge() {
        let got = parsed("#!debate anthropic openai openrouter rounds:2 judge:anthropic#! hello");
        assert_eq!(
            got.mode,
            RunMode::Debate {
                providers: vec![
                    "anthropic".to_string(),
                    "openai".to_string(),
                    "openrouter".to_string()
                ]
            }
        );
        assert_eq!(got.rounds, 2);
        assert_eq!(got.judge, Some("anthropic".to_string()));
    }

    #[test]
    fn tools_none() {
        let got = parsed("#!consensus anthropic openai tools:none#! hello");
        assert_eq!(got.tools, ToolMode::None);
    }

    #[test]
    fn tools_allow_list() {
        let got = parsed("#!consensus anthropic openai tools:read,fetch_page#! hello");
        assert_eq!(
            got.tools,
            ToolMode::AllowList(vec!["read".to_string(), "fetch_page".to_string()])
        );
    }

    #[test]
    fn format_md() {
        let got = parsed("#!anthropic format:md#! summarize this");
        assert_eq!(got.format, OutputFormat::Md);
    }

    #[test]
    fn format_html() {
        let got = parsed("#!anthropic format:html#! summarize this");
        assert_eq!(got.format, OutputFormat::Html);
    }

    #[test]
    fn incompatible_modes_fail() {
        let got = parse_prompt("#!consensus debate anthropic openai#! hello");
        assert!(got.is_err());
    }

    #[test]
    fn judge_on_single_provider_fails() {
        let got = parse_prompt("#!anthropic judge:openai#! hello");
        assert!(got.is_err());
    }

    #[test]
    fn rounds_on_consensus_fails() {
        let got = parse_prompt("#!consensus anthropic openai rounds:2#! hello");
        assert!(got.is_err());
    }

    #[test]
    fn missing_close_marker_fails() {
        let got = parse_prompt("#!anthropic hello");
        assert!(got.is_err());
    }

    #[test]
    fn empty_directive_fails() {
        let got = parse_prompt("#!#!#! hello");
        // "!" between markers is an unknown token
        assert!(got.is_err());
    }

    #[test]
    fn empty_body_fails() {
        let got = parse_prompt("#!anthropic#!");
        assert!(got.is_err());
    }

    #[test]
    fn no_providers_no_mode_is_plain() {
        let got = parsed("#!format:md#! summarize this file");
        assert_eq!(got.mode, RunMode::Single { provider: None });
        assert_eq!(got.format, OutputFormat::Md);
    }

    #[test]
    fn model_override_single_provider() {
        let got = parsed("#!openrouter model:deepseek/deepseek-v3-0324#! explain this");
        assert_eq!(
            got.mode,
            RunMode::Single {
                provider: Some("openrouter".to_string())
            }
        );
        assert_eq!(got.model, Some("deepseek/deepseek-v3-0324".to_string()));
        assert_eq!(got.prompt, "explain this");
    }

    #[test]
    fn model_override_no_provider() {
        let got = parsed("#!model:deepseek/deepseek-v3-0324#! explain this");
        assert_eq!(got.mode, RunMode::Single { provider: None });
        assert_eq!(got.model, Some("deepseek/deepseek-v3-0324".to_string()));
    }

    #[test]
    fn model_override_in_consensus_fails() {
        let got = parse_prompt("#!consensus anthropic openai model:gpt-4o#! hello");
        assert!(got.is_err());
    }

    #[test]
    fn tools_allowlist_double_comma_filters_empty() {
        // Double comma should not produce an empty-string entry — it's silently
        // collapsed, giving the same result as a single comma.
        let got = parsed("#!consensus anthropic openai tools:read,,bash#! hello");
        assert_eq!(
            got.tools,
            ToolMode::AllowList(vec!["read".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn tools_allowlist_only_commas_errors() {
        // A value of only commas produces no valid names after filtering.
        let got = parse_prompt("#!tools:,#! hello");
        assert!(got.is_err());
    }

    #[test]
    fn tools_on_single_provider_fails() {
        // Only consensus/debate orchestration consumes tools: — reject it
        // elsewhere instead of silently ignoring it.
        let got = parse_prompt("#!anthropic tools:none#! hello");
        assert!(got.is_err());
    }

    fn parsed(input: &str) -> PromptDirectives {
        match parse_prompt(input) {
            Ok(value) => value,
            Err(e) => panic!("failed to parse prompt: {e:#}"),
        }
    }
}
