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
        "clear" => Some(Command::Clear),
        "model" => Some(Command::Model(argument)),
        _ => None,
    }
}

async fn run_command(agent: &mut Agent, command: Command<'_>) {
    match command {
        Command::Clear => {
            if let Err(e) = agent.clear_session() {
                output::fail(&format!("{e:#}"));
            }
        }
        Command::Model(alias) => switch_model(agent, alias).await,
    }
}

/// Lasts for this session; the config file is not touched, so what the next
/// run starts on stays something the user wrote.
async fn switch_model(agent: &Agent, requested: Option<&str>) {
    let alias = match requested {
        Some(alias) => alias.to_string(),
        None => {
            output::head("Models");
            match agent.models().pick_any().await {
                Ok(alias) => alias,
                // Cancelling is an ordinary outcome, not a failure.
                Err(e) => return output::note(&format!("{e:#}")),
            }
        }
    };
    match agent.models().set_active(&alias) {
        Ok(()) => {
            output::step(&format!("model: {}", agent.models().active()));
            output::note(
                "this session only — set \"default\" in ~/.oneloop/config.json to keep it",
            );
        }
        Err(e) => output::fail(&format!("{e:#}")),
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
                // A one-shot run must not be silent about which model it is
                // spending on.
                output::step(&format!("{}", agent.models().active()));
                agent.run_once(prompt).await
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

        if let Some(command) = parse_command(&line) {
            run_command(agent, command).await;
            eprintln!();
            continue;
        }

        run_interactive_turn(agent, &line).await;
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
    use super::{Command, interactive_prompt, parse_command};

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
}
