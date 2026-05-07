use anyhow::{Result, bail};

const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "zai"];
const DEFAULT_ROUNDS: usize = 1;
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
struct DirectiveState {
    provider: Option<String>,
    consensus: Option<Vec<String>>,
    debate: Option<Vec<String>>,
    judge: Option<String>,
    rounds: Option<usize>,
    tools: Option<ToolMode>,
}

pub fn parse_prompt(input: &str) -> Result<PromptDirectives> {
    let trimmed = input.trim();
    if !trimmed.starts_with("#!") {
        return Ok(PromptDirectives {
            mode: RunMode::Single { provider: None },
            judge: None,
            rounds: DEFAULT_ROUNDS,
            tools: ToolMode::Default,
            prompt: trimmed.to_string(),
        });
    }

    let mut state = DirectiveState::default();
    let mut body_lines = Vec::new();
    let mut in_directives = true;

    for line in trimmed.lines() {
        let line = line.trim_end();
        if in_directives && line.starts_with("#!") {
            if let Some(inline_body) = parse_directive_line(line, &mut state)? {
                in_directives = false;
                body_lines.push(inline_body);
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

    let mode = resolve_mode(&state)?;
    validate_directives(&mode, &state)?;

    Ok(PromptDirectives {
        mode,
        judge: state.judge,
        rounds: state.rounds.unwrap_or(DEFAULT_ROUNDS),
        tools: state.tools.unwrap_or(ToolMode::Default),
        prompt,
    })
}

fn parse_directive_line(line: &str, state: &mut DirectiveState) -> Result<Option<String>> {
    let rest = line
        .strip_prefix("#!")
        .map(str::trim)
        .filter(|rest| !rest.is_empty());
    let Some(rest) = rest else {
        bail!("empty directive");
    };

    let mut parts = rest.split_whitespace();
    let Some(name) = parts.next() else {
        bail!("empty directive");
    };
    let args: Vec<&str> = parts.collect();

    match name {
        "provider" => parse_provider_directive(&args, state),
        provider if is_known_provider(provider) => parse_provider_shorthand(provider, &args, state),
        "consensus" => parse_multi_provider_directive(&args, |providers| {
            state.consensus = Some(providers);
        }),
        "debate" => parse_multi_provider_directive(&args, |providers| {
            state.debate = Some(providers);
        }),
        "judge" => {
            parse_judge_directive(&args, state)?;
            Ok(None)
        }
        "rounds" => {
            parse_rounds_directive(&args, state)?;
            Ok(None)
        }
        "tools" => {
            parse_tools_directive(&args, state)?;
            Ok(None)
        }
        other => bail!("unknown directive: {other}"),
    }
}

fn parse_provider_directive(args: &[&str], state: &mut DirectiveState) -> Result<Option<String>> {
    let Some(provider) = args.first() else {
        bail!("#!provider requires a provider name");
    };
    state.provider = Some((*provider).to_string());
    Ok(inline_body(args, 1))
}

fn parse_provider_shorthand(
    provider: &str,
    args: &[&str],
    state: &mut DirectiveState,
) -> Result<Option<String>> {
    let provider_names: Vec<String> = std::iter::once(provider)
        .chain(
            args.iter()
                .copied()
                .take_while(|arg| is_known_provider(arg)),
        )
        .map(ToString::to_string)
        .collect();

    if provider_names.len() == 1 {
        state.provider = Some(provider.to_string());
        Ok(inline_body(args, 0))
    } else {
        let provider_count = provider_names.len();
        state.consensus = Some(provider_names);
        Ok(inline_body(args, provider_count - 1))
    }
}

fn parse_multi_provider_directive<F>(args: &[&str], mut set_providers: F) -> Result<Option<String>>
where
    F: FnMut(Vec<String>),
{
    let provider_count = args.iter().take_while(|arg| is_known_provider(arg)).count();
    if provider_count < 2 {
        bail!("multi-model directives require at least two provider names");
    }

    let providers = args
        .iter()
        .take(provider_count)
        .map(ToString::to_string)
        .collect();
    set_providers(providers);
    Ok(inline_body(args, provider_count))
}

fn parse_judge_directive(args: &[&str], state: &mut DirectiveState) -> Result<()> {
    match args {
        [provider] => {
            state.judge = Some((*provider).to_string());
            Ok(())
        }
        [] => bail!("#!judge requires a provider name"),
        _ => bail!("#!judge accepts exactly one provider name"),
    }
}

fn parse_rounds_directive(args: &[&str], state: &mut DirectiveState) -> Result<()> {
    match args {
        [rounds] => {
            let rounds = rounds
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!("#!rounds must be a positive integer"))?;
            if rounds == 0 || rounds > MAX_ROUNDS {
                bail!("#!rounds must be between 1 and {MAX_ROUNDS}");
            }
            state.rounds = Some(rounds);
            Ok(())
        }
        [] => bail!("#!rounds requires a positive integer"),
        _ => bail!("#!rounds accepts exactly one positive integer"),
    }
}

fn parse_tools_directive(args: &[&str], state: &mut DirectiveState) -> Result<()> {
    if args.is_empty() {
        bail!("#!tools requires `none` or a list of tool names");
    }

    state.tools = Some(if args == ["none"] {
        ToolMode::None
    } else {
        ToolMode::AllowList(args.iter().map(ToString::to_string).collect())
    });
    Ok(())
}

fn resolve_mode(state: &DirectiveState) -> Result<RunMode> {
    let modes = [
        state.provider.as_ref().map(|_| "#!provider"),
        state.consensus.as_ref().map(|_| "#!consensus"),
        state.debate.as_ref().map(|_| "#!debate"),
    ];
    let active_modes: Vec<&str> = modes.into_iter().flatten().collect();

    if active_modes.len() > 1 {
        bail!("incompatible directives: {}", active_modes.join(", "));
    }

    if let Some(provider) = &state.provider {
        return Ok(RunMode::Single {
            provider: Some(provider.clone()),
        });
    }
    if let Some(providers) = &state.consensus {
        return Ok(RunMode::Consensus {
            providers: providers.clone(),
        });
    }
    if let Some(providers) = &state.debate {
        return Ok(RunMode::Debate {
            providers: providers.clone(),
        });
    }

    Ok(RunMode::Single { provider: None })
}

fn validate_directives(mode: &RunMode, state: &DirectiveState) -> Result<()> {
    match mode {
        RunMode::Single { .. } => {
            if state.judge.is_some() {
                bail!("#!judge is only valid with #!consensus or #!debate");
            }
            if state.rounds.is_some() {
                bail!("#!rounds is only valid with #!debate");
            }
        }
        RunMode::Consensus { .. } => {
            if state.rounds.is_some() {
                bail!("#!rounds is only valid with #!debate");
            }
        }
        RunMode::Debate { .. } => {}
    }
    Ok(())
}

fn inline_body(args: &[&str], start: usize) -> Option<String> {
    (args.len() > start).then(|| args[start..].join(" "))
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
