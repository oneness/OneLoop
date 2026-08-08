use anyhow::Result;
use rustyline::error::ReadlineError;

use crate::output::{BOLD, DIM, RED, RESET, YELLOW};
use crate::{
    agent::Agent,
    config::{self, Config},
    models::ModelRegistry,
    tools::ToolRegistry,
};

pub struct App {
    config: Config,
}

/// A line the REPL answers itself instead of sending to a model.
enum Command<'a> {
    Clear,
    /// Summarize now, rather than waiting for a model to refuse the next
    /// request. Nothing else compacts on a schedule.
    Compact,
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
        "compact" => Some(Command::Compact),
        "model" => Some(Command::Model(argument)),
        _ => None,
    }
}

async fn run_command(agent: &mut Agent, command: Command<'_>) {
    match command {
        Command::Clear => {
            if let Err(e) = agent.clear_session() {
                eprintln!("{RED}  ✗ {e:#}{RESET}");
            }
        }
        // A failed summary is reported where it happens, and leaves the
        // session untouched — there is nothing to add here.
        Command::Compact => {
            if let Err(e) = agent.compact(None).await {
                eprintln!("{RED}  ✗ {e:#}{RESET}");
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
            println!("{BOLD}  ── Models ──{RESET}");
            match agent.models().pick_any().await {
                Ok(alias) => alias,
                // Cancelling is an ordinary outcome, not a failure.
                Err(e) => return println!("{DIM}  {e:#}{RESET}"),
            }
        }
    };
    match agent.models().set_active(&alias) {
        Ok(()) => {
            println!("{DIM}  → model: {}{RESET}", agent.models().active());
            println!(
                "{DIM}  this session only — set \"default\" in ~/.oneloop/config.json to keep it{RESET}"
            );
        }
        Err(e) => eprintln!("{RED}  ✗ {e:#}{RESET}"),
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
                eprintln!("{DIM}  → {}{RESET}", agent.models().active());
                agent.run_once(prompt).await
            }
            None => run_interactive(&mut agent).await,
        }
    }
}

/// The interactive REPL: read a line, run it, repeat until Ctrl+D.
async fn run_interactive(agent: &mut Agent) -> Result<()> {
    println!("OneLoop");
    println!("{}", agent.summary());
    println!();
    println!(
        "interactive mode — type your message, /model to switch model, /compact to summarize, /clear to reset context, Ctrl+C to stop"
    );
    println!();

    // Canonical mode silently drops input past the tty's 4096-byte line
    // buffer and locks up the prompt on long pastes.
    let mut editor = rustyline::DefaultEditor::new()?;

    loop {
        let line = match editor.readline("> ") {
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
            println!();
            continue;
        }

        run_interactive_turn(agent, &line).await;
        println!();
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
                eprintln!("{RED}  ✗ {e:#}{RESET}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            interrupted = true;
            println!("{YELLOW}  ⏹ stopped{RESET}");
        }
    }

    // A dropped run leaves tool calls without results, which providers
    // reject on every later request in this session.
    if interrupted && let Err(e) = agent.repair_dangling_tool_calls() {
        eprintln!("{RED}  ✗ session repair failed: {e:#}{RESET}");
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, parse_command};

    #[test]
    fn clear_is_a_command() {
        assert!(matches!(parse_command("/clear"), Some(Command::Clear)));
    }

    #[test]
    fn compact_is_a_command() {
        // The only way to compact on purpose: nothing does it on a timer.
        assert!(matches!(parse_command("/compact"), Some(Command::Compact)));
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
