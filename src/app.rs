use anyhow::Result;
use rustyline::error::ReadlineError;

use crate::{
    agent::Agent,
    config::Config,
    directives::{OutputFormat, PromptDirectives, RunMode, parse_prompt},
    providers::ProviderRegistry,
    tools::ToolRegistry,
};

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
            eprintln!("\x1b[90m  → provider: {provider}\x1b[0m");
        }
        RunMode::Single { provider: None } => {}
        RunMode::Consensus { providers } => {
            eprintln!("\x1b[90m  → consensus: {}\x1b[0m", providers.join(", "));
        }
        RunMode::Debate { providers } => {
            eprintln!("\x1b[90m  → debate: {}\x1b[0m", providers.join(", "));
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

    pub async fn run(self, prompt: Option<String>) -> Result<()> {
        let provider_registry = ProviderRegistry::new()?;
        let tool_registry = ToolRegistry::with_builtin_tools(&self.config.cwd)?;
        let mut agent = Agent::new(self.config, provider_registry, tool_registry)?;

        match prompt {
            Some(prompt) => run_directed_prompt(&mut agent, &prompt).await,
            None => {
                println!("OneLoop");
                println!("{}", agent.summary());
                println!();
                println!(
                    "interactive mode — type your message, /clear to reset context, Ctrl+C to stop"
                );
                println!();

                // A raw-mode line editor instead of stdin's canonical mode,
                // which silently drops input past the tty's 4096-byte line
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

                    // Check for built-in commands.
                    if let Some(ReplCommand::Clear) = parse_command(&line) {
                        agent.clear_session()?;
                        println!();
                        continue;
                    }

                    let directives = match parse_prompt(&line) {
                        Ok(directives) => directives,
                        Err(e) => {
                            eprintln!("\x1b[31m  ✗ {e:#}\x1b[0m");
                            println!(
                                "\x1b[90m  hint: use #!directive words#! <your message>, e.g. #!anthropic#! explain this file\x1b[0m"
                            );
                            println!();
                            continue;
                        }
                    };
                    let compact_provider_override =
                        provider_override(&directives).map(String::from);

                    // Use select to race the agent run against Ctrl+C.
                    // Ctrl+C drops the run future mid-flight.
                    let mut interrupted = false;
                    tokio::select! {
                        result = run_directives(&mut agent, directives) => {
                            if let Err(e) = result {
                                eprintln!("\x1b[31m  ✗ {e:#}\x1b[0m");
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            interrupted = true;
                            println!("\x1b[33m  ⏹ stopped\x1b[0m");
                        }
                    }

                    // A dropped run may have recorded tool calls whose results
                    // never arrived; close them out or providers will reject
                    // every later request in this session.
                    if interrupted && let Err(e) = agent.repair_dangling_tool_calls() {
                        eprintln!("\x1b[31m  ✗ session repair failed: {e:#}\x1b[0m");
                    }

                    // Auto-compact if context is near limit. Failure is not
                    // fatal to the REPL — report it and keep going.
                    if let Err(e) = agent
                        .auto_compact_if_needed(compact_provider_override.as_deref())
                        .await
                    {
                        eprintln!("\x1b[31m  ✗ auto-compaction failed: {e:#}\x1b[0m");
                    }

                    println!();
                }

                Ok(())
            }
        }
    }
}
