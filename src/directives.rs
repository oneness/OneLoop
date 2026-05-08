use anyhow::{Result, bail};

const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "zai"];
const MAX_ROUNDS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDirectives {
    pub mode: RunMode,
    pub judge: Option<String>,
    pub rounds: usize,
    pub tools: ToolMode,
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

#[derive(Debug, Default)]
struct State {
    mode: Option<RunMode>,
    judge: Option<String>,
    rounds: Option<usize>,
    tools: Option<ToolMode>,
}

impl State {
    fn set_mode(&mut self, mode: RunMode) -> Result<()> {
        if self.mode.is_some() {
            bail!("incompatible directives: only one mode allowed");
        }
        self.mode = Some(mode);
        Ok(())
    }
}

pub fn parse_prompt(input: &str) -> Result<PromptDirectives> {
    let trimmed = input.trim();
    if !trimmed.starts_with("#!") {
        return Ok(PromptDirectives {
            mode: RunMode::Single { provider: None },
            judge: None,
            rounds: 1,
            tools: ToolMode::Default,
            prompt: trimmed.to_string(),
        });
    }

    let mut state = State::default();
    let mut body_lines: Vec<String> = Vec::new();
    let mut in_directives = true;

    for line in trimmed.lines() {
        let line = line.trim_end();
        if in_directives && line.starts_with("#!") {
            if let Some(inline) = parse_directive_line(line, &mut state)? {
                in_directives = false;
                body_lines.push(inline);
            }
        } else if in_directives && line.trim().is_empty() {
            continue;
        } else {
            in_directives = false;
            body_lines.push(line.to_string());
        }
    }

    let prompt = body_lines.join("\n").trim().to_string();
    if prompt.is_empty() {
        bail!("directive prompt body is empty");
    }

    validate(&state)?;

    Ok(PromptDirectives {
        mode: state.mode.unwrap_or(RunMode::Single { provider: None }),
        judge: state.judge,
        rounds: state.rounds.unwrap_or(1),
        tools: state.tools.unwrap_or(ToolMode::Default),
        prompt,
    })
}

/// Split tokens into (known providers, remaining words as inline body).
fn split_providers(tokens: &[&str]) -> (Vec<String>, Option<String>) {
    let n = tokens
        .iter()
        .take_while(|t| is_known_provider(t))
        .count();
    let providers: Vec<String> = tokens[..n].iter().map(ToString::to_string).collect();
    let body = (tokens.len() > n).then(|| tokens[n..].join(" "));
    (providers, body)
}

/// Join remaining tokens as inline body.
fn body_from(tokens: &[&str]) -> Option<String> {
    (!tokens.is_empty()).then(|| tokens.join(" "))
}

fn parse_directive_line(line: &str, state: &mut State) -> Result<Option<String>> {
    let rest = line[2..].trim();
    if rest.is_empty() {
        bail!("empty directive");
    }

    let mut parts = rest.split_whitespace();
    let Some(name) = parts.next() else {
        bail!("empty directive");
    };
    let args: Vec<&str> = parts.collect();

    match name {
        // Explicit #!provider — first arg is provider, rest is body
        "provider" => {
            let provider = args
                .first()
                .ok_or_else(|| anyhow::anyhow!("#!provider requires a provider name"))?;
            state.set_mode(RunMode::Single {
                provider: Some(provider.to_string()),
            })?;
            Ok(body_from(&args[1..]))
        }

        // Explicit #!consensus / #!debate — greedily collect providers from args
        "consensus" | "debate" => {
            let (providers, body) = split_providers(&args);
            if providers.len() < 2 {
                bail!("#!{name} requires at least two provider names");
            }
            state.set_mode(if name == "consensus" {
                RunMode::Consensus { providers }
            } else {
                RunMode::Debate { providers }
            })?;
            Ok(body)
        }

        // Provider shorthand — greedy providers from full token list
        p if is_known_provider(p) => {
            let tokens: Vec<&str> = std::iter::once(p).chain(args).collect();
            let (providers, body) = split_providers(&tokens);
            state.set_mode(if providers.len() == 1 {
                RunMode::Single {
                    provider: providers.into_iter().next(),
                }
            } else {
                RunMode::Consensus { providers }
            })?;
            Ok(body)
        }

        "judge" => match args.as_slice() {
            [p] => {
                state.judge = Some(p.to_string());
                Ok(None)
            }
            [] => bail!("#!judge requires a provider name"),
            _ => bail!("#!judge accepts exactly one provider name"),
        },

        "rounds" => match args.as_slice() {
            [n] => {
                let r = n
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("#!rounds must be a positive integer"))?;
                if r == 0 || r > MAX_ROUNDS {
                    bail!("#!rounds must be between 1 and {MAX_ROUNDS}");
                }
                state.rounds = Some(r);
                Ok(None)
            }
            [] => bail!("#!rounds requires a positive integer"),
            _ => bail!("#!rounds accepts exactly one positive integer"),
        },

        "tools" => {
            if args.is_empty() {
                bail!("#!tools requires `none` or a list of tool names");
            }
            state.tools = Some(if args == ["none"] {
                ToolMode::None
            } else {
                ToolMode::AllowList(args.iter().map(ToString::to_string).collect())
            });
            Ok(None)
        }

        other => bail!("unknown directive: {other}"),
    }
}

fn validate(state: &State) -> Result<()> {
    let is_multi = matches!(
        state.mode,
        Some(RunMode::Consensus { .. }) | Some(RunMode::Debate { .. })
    );
    let is_debate = matches!(state.mode, Some(RunMode::Debate { .. }));

    if state.judge.is_some() && !is_multi {
        bail!("#!judge is only valid with #!consensus or #!debate");
    }
    if state.rounds.is_some() && !is_debate {
        bail!("#!rounds is only valid with #!debate");
    }
    Ok(())
}

fn is_known_provider(value: &str) -> bool {
    KNOWN_PROVIDERS.contains(&value)
}

#[cfg(test)]
mod tests {
    use super::{PromptDirectives, RunMode, ToolMode, parse_prompt};

    #[test]
    fn plain_prompt_uses_default_single_mode() {
        let got = parsed("hello");
        assert_eq!(got.mode, RunMode::Single { provider: None });
    }

    #[test]
    fn provider_directive_routes_single_mode() {
        let got = parsed("#!provider anthropic\nhello");
        assert_eq!(
            got.mode,
            RunMode::Single {
                provider: Some("anthropic".to_string())
            }
        );
    }

    #[test]
    fn provider_directive_supports_inline_body() {
        let got = parsed("#!provider anthropic hello there");
        assert_eq!(got.prompt, "hello there");
    }

    #[test]
    fn consensus_directive_collects_providers() {
        let got = parsed("#!consensus anthropic openai\nhello");
        assert_eq!(
            got.mode,
            RunMode::Consensus {
                providers: vec!["anthropic".to_string(), "openai".to_string()]
            }
        );
    }

    #[test]
    fn consensus_directive_supports_inline_body() {
        let got = parsed("#!consensus anthropic openai should we do this");
        assert_eq!(got.prompt, "should we do this");
    }

    #[test]
    fn debate_directive_uses_rounds() {
        let got = parsed("#!debate anthropic openai\n#!rounds 2\nhello");
        assert_eq!(got.rounds, 2);
    }

    #[test]
    fn tools_none_is_parsed() {
        let got = parsed("#!consensus anthropic openai\n#!tools none\nhello");
        assert_eq!(got.tools, ToolMode::None);
    }

    #[test]
    fn incompatible_modes_fail() {
        let got = parse_prompt("#!provider anthropic\n#!consensus anthropic openai\nhello");
        assert!(got.is_err());
    }

    #[test]
    fn provider_shorthand_routes_single_provider() {
        let got = parsed("#!anthropic hello");
        assert_eq!(
            got.mode,
            RunMode::Single {
                provider: Some("anthropic".to_string())
            }
        );
    }

    #[test]
    fn provider_shorthand_routes_multiple_providers_to_consensus() {
        let got = parsed("#!anthropic openai should we do this");
        assert_eq!(
            got.mode,
            RunMode::Consensus {
                providers: vec!["anthropic".to_string(), "openai".to_string()]
            }
        );
    }

    #[test]
    fn provider_shorthand_consensus_preserves_inline_body() {
        let got = parsed("#!anthropic openai should we do this");
        assert_eq!(got.prompt, "should we do this");
    }

    fn parsed(input: &str) -> PromptDirectives {
        match parse_prompt(input) {
            Ok(value) => value,
            Err(e) => panic!("failed to parse prompt: {e:#}"),
        }
    }
}
