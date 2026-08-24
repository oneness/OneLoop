use std::borrow::Cow;

use anyhow::Result;
use rustyline::{
    Editor, Helper, completion::Completer, error::ReadlineError, highlight::Highlighter,
    hint::Hinter, history::DefaultHistory, validate::Validator,
};

use crate::output;
use crate::{
    agent::Agent,
    config::{self, Config},
    models::ModelRegistry,
    tools::ToolRegistry,
};

pub struct App {
    config: Config,
}

struct ReplHelper;

impl Completer for ReplHelper {
    type Candidate = String;
}

impl Hinter for ReplHelper {
    type Hint = String;
}

impl Highlighter for ReplHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!("{}{prompt}{}", output::BOLD, output::RESET))
    }
}

impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

fn interactive_prompt(alias: &str) -> String {
    format!("({alias})> ")
}

/// A line the REPL answers itself instead of sending to a model.
enum Command<'a> {
    Clear,
    /// An alias switches straight to it; without one, the list is offered.
    Model(Option<&'a str>),
}

/// An unknown `/word` is a prompt, not an error: a message can legitimately
/// start with a path.
fn parse_command(line: &str) -> Option<Command<'_>> {
    let (name, argument) = match line.strip_prefix('/')?.split_once(char::is_whitespace) {
        Some((name, argument)) => (name, argument.trim()),
        None => (line.trim_start_matches('/'), ""),
    };
    let argument = (!argument.is_empty()).then_some(argument);
    match name {
        "clear" if argument.is_none() => Some(Command::Clear),
        "model" => Some(Command::Model(argument)),
        _ => None,
    }
}

struct ParsedInput<'a> {
    prompt: Option<&'a str>,
    command: Option<Command<'a>>,
}

/// A prefixed command uses `.` as an explicit boundary:
/// `/clear. explain this` and `/model flash. explain this`.
fn parse_input(input: &str) -> ParsedInput<'_> {
    let trimmed = input.trim();
    if let Some((prompt, command)) = parse_prefixed_command(trimmed) {
        return ParsedInput {
            prompt: Some(prompt),
            command: Some(command),
        };
    }
    if let Some(command) = parse_command(trimmed) {
        return ParsedInput {
            prompt: None,
            command: Some(command),
        };
    }

    ParsedInput {
        prompt: Some(input),
        command: None,
    }
}

fn parse_prefixed_command(input: &str) -> Option<(&str, Command<'_>)> {
    input
        .strip_prefix("/clear")
        .and_then(prompt_after_separator)
        .map(|prompt| (prompt, Command::Clear))
        .or_else(|| {
            let remainder = input.strip_prefix("/model")?;
            let remainder = remainder.strip_prefix(char::is_whitespace)?.trim_start();
            let (alias, prompt) = remainder.split_once('.')?;
            let alias = alias.trim();
            let prompt = prompt.trim();
            (!alias.is_empty() && !prompt.is_empty())
                .then_some((prompt, Command::Model(Some(alias))))
        })
}

fn prompt_after_separator(remainder: &str) -> Option<&str> {
    remainder.strip_prefix('.').and_then(nonempty_trimmed)
}

fn nonempty_trimmed(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Returns false when the command failed or selection was cancelled, so a
/// prompt attached to it is never sent with unintended state.
async fn run_command(agent: &mut Agent, command: Command<'_>) -> bool {
    match command {
        Command::Clear => match agent.clear_session() {
            Ok(()) => true,
            Err(e) => {
                output::fail(&format!("{e:#}"));
                false
            }
        },
        Command::Model(alias) => switch_model(agent, alias).await,
    }
}

/// Lasts for this session; the config file is not touched, so what the next
/// run starts on stays something the user wrote.
async fn switch_model(agent: &Agent, requested: Option<&str>) -> bool {
    let alias = match requested {
        Some(alias) => alias.to_string(),
        None => {
            output::head("Models");
            match agent.models().pick_any().await {
                Ok(alias) => alias,
                // Cancelling is an ordinary outcome, not a failure.
                Err(e) => {
                    output::note(&format!("{e:#}"));
                    return false;
                }
            }
        }
    };
    match agent.models().set_active(&alias) {
        Ok(()) => {
            output::step(&format!("model: {}", agent.models().active()));
            output::note(
                "this session only — set \"default\" in ~/.oneloop/config.json to keep it",
            );
            true
        }
        Err(e) => {
            output::fail(&format!("{e:#}"));
            false
        }
    }
}

impl App {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(mut self, prompt: Option<String>) -> Result<()> {
        let models = ModelRegistry::new()?;
        let tool_registry = ToolRegistry::with_builtin_tools(&self.config.cwd)?;
        self.config.system_prompt = Some(config::build_system_prompt(
            &self.config.cwd,
            &tool_registry.names(),
        ));
        self.config.prompt_sources = config::prompt_sources(&self.config.cwd);
        let mut agent = Agent::new(self.config, models, tool_registry)?;

        match prompt {
            Some(prompt) => {
                let parsed = parse_input(&prompt);
                if let Some(command) = parsed.command
                    && !run_command(&mut agent, command).await
                {
                    return Ok(());
                }
                let Some(prompt) = parsed.prompt else {
                    return Ok(());
                };
                // A one-shot run must not be silent about which model it is
                // spending on.
                output::step(&format!("{}", agent.models().active()));
                agent.run_once(prompt.to_string()).await
            }
            None => run_interactive(&mut agent).await,
        }
    }
}

/// The interactive REPL: read a line, run it, repeat until Ctrl+D.
async fn run_interactive(agent: &mut Agent) -> Result<()> {
    // The banner is OneLoop talking about itself, so it goes where the rest
    // of that goes — stdout stays the model's.
    eprintln!("OneLoop");
    eprintln!("{}", agent.summary());
    eprintln!();
    eprintln!(
        "interactive mode — type your message, /model to switch model, /clear to reset context, Ctrl+C to stop"
    );
    eprintln!();

    // Canonical mode silently drops input past the tty's 4096-byte line
    // buffer and locks up the prompt on long pastes.
    let mut editor = Editor::<ReplHelper, DefaultHistory>::new()?;
    editor.set_helper(Some(ReplHelper));

    loop {
        let prompt = interactive_prompt(&agent.models().active().alias);
        let line = match editor.readline(&prompt) {
            Ok(input) => input.trim().to_string(),
            // Ctrl+C at the prompt discards the current line.
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl+D exits.
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(&line);

        let parsed = parse_input(&line);
        if let Some(command) = parsed.command
            && !run_command(agent, command).await
        {
            eprintln!();
            continue;
        }
        let Some(prompt) = parsed.prompt else {
            eprintln!();
            continue;
        };

        run_interactive_turn(agent, prompt).await;
        eprintln!();
    }

    Ok(())
}

/// Errors are reported, never propagated — a failed turn must not end the
/// REPL.
async fn run_interactive_turn(agent: &mut Agent, line: &str) {
    // Ctrl+C drops the run future mid-flight.
    let mut interrupted = false;
    tokio::select! {
        result = agent.run_once(line.to_string()) => {
            if let Err(e) = result {
                output::fail(&format!("{e:#}"));
            }
        }
        _ = tokio::signal::ctrl_c() => {
            interrupted = true;
            output::stopped("stopped");
        }
    }

    // A dropped run leaves tool calls without results, which providers
    // reject on every later request in this session.
    if interrupted && let Err(e) = agent.repair_dangling_tool_calls() {
        output::fail(&format!("session repair failed: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, interactive_prompt, parse_command, parse_input};

    #[test]
    fn interactive_prompt_names_the_active_model() {
        assert_eq!(interactive_prompt("qwen"), "(qwen)> ");
    }

    #[test]
    fn clear_is_a_command() {
        assert!(matches!(parse_command("/clear"), Some(Command::Clear)));
    }

    #[test]
    fn model_takes_an_optional_alias() {
        assert!(matches!(
            parse_command("/model"),
            Some(Command::Model(None))
        ));
        assert!(matches!(
            parse_command("/model flash"),
            Some(Command::Model(Some("flash")))
        ));
    }

    #[test]
    fn a_message_starting_with_a_path_is_not_a_command() {
        // Refusing unknown /words would swallow this as a typo.
        assert!(parse_command("/etc/hosts is unreadable, why?").is_none());
    }

    #[test]
    fn prefixed_clear_is_extracted() {
        let parsed = parse_input("/clear. Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("Explain this"), Some(Command::Clear))
        ));
    }

    #[test]
    fn prefixed_model_is_extracted() {
        let parsed = parse_input("/model flash. Why does this fail?");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (
                Some("Why does this fail?"),
                Some(Command::Model(Some("flash")))
            )
        ));
    }

    #[test]
    fn spoken_clear_stays_in_the_prompt() {
        let parsed = parse_input("slash clear dot Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("slash clear dot Explain this"), None)
        ));
    }

    #[test]
    fn spoken_model_stays_in_the_prompt() {
        let parsed = parse_input("slash model flash dot Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("slash model flash dot Explain this"), None)
        ));
    }

    #[test]
    fn typed_command_with_spoken_dot_stays_in_the_prompt() {
        let parsed = parse_input("/clear dot Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/clear dot Explain this"), None)
        ));
    }

    #[test]
    fn prefix_without_separator_stays_in_the_prompt() {
        let parsed = parse_input("/clear Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/clear Explain this"), None)
        ));
    }

    #[test]
    fn old_trailing_form_stays_in_the_prompt() {
        let parsed = parse_input("Explain this. /clear");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("Explain this. /clear"), None)
        ));
    }

    #[test]
    fn unknown_prefixed_slash_word_stays_in_the_prompt() {
        let parsed = parse_input("/etc/hosts. Read this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/etc/hosts. Read this"), None)
        ));
    }
}
