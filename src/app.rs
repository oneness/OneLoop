use anyhow::Result;
use rustyline::error::ReadlineError;

use crate::{
    agent::Agent,
    config::{self, Config},
    directives::{OutputFormat, PromptDirectives, RunMode, parse_prompt},
    providers::ProviderRegistry,
    tools::ToolRegistry,
};
use crate::output::{DIM, RED, RESET, YELLOW};

pub struct App {
    config: Config,
}

/// Built-in interactive commands, entered with a leading `/`.
enum ReplCommand {
    Clear,
}

fn parse_command(input: &str) -> Option<ReplCommand> {
    match input.trim() {
        "/clear" => Some(ReplCommand::Clear),
        _ => None,
    }
}

fn print_directive_summary(directives: &PromptDirectives) {
    match &directives.mode {
        RunMode::Single {
            provider: Some(provider),
        } => {
            eprintln!("{DIM}  → provider: {provider}{RESET}");
        }
        RunMode::Single { provider: None } => {}
        RunMode::Consensus { providers } => {
            eprintln!("{DIM}  → consensus: {}{RESET}", providers.join(", "));
        }
        RunMode::Debate { providers } => {
            eprintln!("{DIM}  → debate: {}{RESET}", providers.join(", "));
        }
    }
}

async fn run_directives(agent: &mut Agent, directives: PromptDirectives) -> Result<()> {
    print_directive_summary(&directives);
    let prompt = with_format_instruction(directives.prompt, &directives.format);
    match directives.mode {
        RunMode::Single { provider } => {
            agent
                .run_once_with(prompt, provider.as_deref(), directives.model)
                .await
        }
        RunMode::Consensus { providers } => {
            agent
                .run_consensus(prompt, providers, directives.judge, directives.tools)
                .await
        }
        RunMode::Debate { providers } => {
            agent
                .run_debate(
                    prompt,
                    providers,
                    directives.judge,
                    directives.rounds,
                    directives.tools,
                )
                .await
        }
    }
}

/// Turn the `format:` directive into a plain instruction appended to the
/// prompt — the model does the formatting; there is no post-processing.
fn with_format_instruction(prompt: String, format: &OutputFormat) -> String {
    let instruction = match format {
        OutputFormat::Plain => return prompt,
        OutputFormat::Md => "Format your final answer as Markdown.",
        OutputFormat::Html => "Format your final answer as a single self-contained HTML document.",
    };
    format!("{prompt}\n\n{instruction}")
}

async fn run_directed_prompt(agent: &mut Agent, input: &str) -> Result<()> {
    run_directives(agent, parse_prompt(input)?).await
}

fn provider_override(directives: &PromptDirectives) -> Option<&str> {
    match &directives.mode {
        RunMode::Single { provider } => provider.as_deref(),
        RunMode::Consensus { .. } | RunMode::Debate { .. } => None,
    }
}

impl App {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(mut self, prompt: Option<String>) -> Result<()> {
        let provider_registry = ProviderRegistry::new()?;
        let tool_registry = ToolRegistry::with_builtin_tools(&self.config.cwd)?;
        self.config.system_prompt =
            config::build_system_prompt(&self.config.cwd, &tool_registry.names());
        let mut agent = Agent::new(self.config, provider_registry, tool_registry)?;

        match prompt {
            Some(prompt) => {
                // Interactive mode prints a full banner; one-shot runs must
                // also never be silent about which model they are spending on.
                eprintln!("{DIM}  → {}{RESET}", agent.provider_line());
                run_directed_prompt(&mut agent, &prompt).await
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
    println!("interactive mode — type your message, /clear to reset context, Ctrl+C to stop");
    println!();

    // A raw-mode line editor instead of stdin's canonical mode, which
    // silently drops input past the tty's 4096-byte line buffer and locks
    // up the prompt on long pastes.
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

        // Check for built-in commands.
        if let Some(ReplCommand::Clear) = parse_command(&line) {
            agent.clear_session()?;
            println!();
            continue;
        }

        run_interactive_turn(agent, &line).await;
        println!();
    }

    Ok(())
}

/// One REPL turn: parse directives, run them racing Ctrl+C, then tidy up.
/// Errors are reported, never propagated — a failed turn must not end the REPL.
async fn run_interactive_turn(agent: &mut Agent, line: &str) {
    let directives = match parse_prompt(line) {
        Ok(directives) => directives,
        Err(e) => {
            eprintln!("{RED}  ✗ {e:#}{RESET}");
            println!(
                "{DIM}  hint: use #!directive words#! <your message>, e.g. #!anthropic#! explain this file{RESET}"
            );
            return;
        }
    };
    let compact_provider_override = provider_override(&directives).map(String::from);

    // Use select to race the agent run against Ctrl+C.
    // Ctrl+C drops the run future mid-flight.
    let mut interrupted = false;
    tokio::select! {
        result = run_directives(agent, directives) => {
            if let Err(e) = result {
                eprintln!("{RED}  ✗ {e:#}{RESET}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            interrupted = true;
            println!("{YELLOW}  ⏹ stopped{RESET}");
        }
    }

    // A dropped run may have recorded tool calls whose results never
    // arrived; close them out or providers will reject every later
    // request in this session.
    if interrupted && let Err(e) = agent.repair_dangling_tool_calls() {
        eprintln!("{RED}  ✗ session repair failed: {e:#}{RESET}");
    }

    // Auto-compact if context is near limit.
    if let Err(e) = agent
        .auto_compact_if_needed(compact_provider_override.as_deref())
        .await
    {
        eprintln!("{RED}  ✗ auto-compaction failed: {e:#}{RESET}");
    }
}
