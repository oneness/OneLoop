use std::borrow::Cow;
use std::cell::RefCell;
use std::env;
use std::rc::Rc;

use anyhow::Result;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};

use crate::agent::Agent;
use crate::bash_history::BashHistory;
use crate::config::Config;
use crate::models::ModelRegistry;
use crate::output;
use crate::prompt;
use crate::tools::ToolRegistry;
use crate::tools::bash;

/// A `!` command runs for the user, with Ctrl+C in reach, so it gets a longer
/// leash than a tool call's 30 seconds.
const BASH_TIMEOUT_SECS: u64 = 300;

/// How many candidates Tab lists. A hint offers the first of them.
const COMPLETION_LIMIT: usize = 5;

/// How many commands `!` on its own shows: the only route to the history on a
/// terminal rustyline cannot edit on.
const RECENT_COMMANDS: usize = 10;

/// How the process was asked to run. The one-shot variant is stateless by
/// definition: it exists only because a prompt was given on the command line
/// or over a pipe.
#[derive(Debug)]
pub enum Run {
    Interactive,
    OneShot(String),
}

pub struct App {
    config: Config,
}

/// Completes `!` lines from the user's bash history, not the REPL's line history.
struct ReplHelper {
    history: Rc<RefCell<BashHistory>>,
}

/// The command under the cursor and where it starts, when the line is a `!`
/// command. Space after the sigil is skipped so `! git` completes the same way
/// `!git` does, while the replacement still begins where the command does.
fn bash_fragment(line: &str, pos: usize) -> Option<(usize, &str)> {
    let after_sigil = line.get(..pos)?.strip_prefix('!')?;
    let fragment = after_sigil.trim_start();
    Some((1 + after_sigil.len() - fragment.len(), fragment))
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    /// The replacement starts after the sigil, so the `!` survives and the
    /// elected command replaces the fragment.
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let Some((start, fragment)) = bash_fragment(line, pos) else {
            return Ok((0, Vec::new()));
        };
        let candidates = self
            .history
            .borrow()
            .matches(fragment, COMPLETION_LIMIT)
            .into_iter()
            .map(|command| Pair {
                display: command.to_string(),
                replacement: command.to_string(),
            })
            .collect();
        Ok((start, candidates))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;

    /// The rest of the newest command starting with what is typed.
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // A hint is drawn after the cursor and accepted by moving right, so it
        // can only speak for the end of the line.
        if pos != line.len() {
            return None;
        }
        let (_, fragment) = bash_fragment(line, pos)?;
        let history = self.history.borrow();
        let best = history.hint_match(fragment)?;
        let remainder = best.get(fragment.len()..)?;
        (!remainder.is_empty()).then(|| remainder.to_string())
    }
}

impl Highlighter for ReplHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!("{}{prompt}{}", output::BOLD, output::RESET))
    }

    /// A hint is the command about to be accepted, not text already typed.
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if output::plain() {
            Cow::Borrowed(hint)
        } else {
            Cow::Owned(format!("{}{hint}{}", output::DIM, output::RESET))
        }
    }
}

impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

fn interactive_prompt(alias: &str, remaining_context: Option<u8>) -> String {
    match remaining_context {
        Some(remaining) => format!("({alias} ~ {remaining}%)> "),
        None => format!("({alias})> "),
    }
}

/// How `/cc`, `/ca`, and `/cn` ask Claude.
#[derive(Debug, PartialEq, Eq)]
enum AskMode {
    Critique,
    Ask,
    Standalone,
}

impl AskMode {
    fn as_command(&self) -> &'static str {
        match self {
            AskMode::Critique => "/cc",
            AskMode::Ask => "/ca",
            AskMode::Standalone => "/cn",
        }
    }

    fn as_antigravity_command(&self) -> &'static str {
        match self {
            AskMode::Critique => "/ag",
            AskMode::Ask => "/aga",
            AskMode::Standalone => "/agn",
        }
    }
}

/// A line the REPL answers itself instead of sending to a model.
enum Command<'a> {
    Help,
    Clear,
    Claude {
        mode: AskMode,
        text: Option<&'a str>,
    },
    Antigravity {
        mode: AskMode,
        text: Option<&'a str>,
    },
    Reload,
    /// An alias switches straight to it; without one, the list is offered.
    Model(Option<&'a str>),
    /// A command the user runs themselves: `!ls`. Recorded, never asked about.
    Bash(&'a str),
}

/// A `!` line is a bash command taken verbatim: unlike `/`, a `;` in it is the
/// shell's separator, so there is no `!cmd; prompt` form.
fn parse_bash(line: &str) -> Option<&str> {
    line.strip_prefix('!').map(str::trim)
}

/// An unknown `/word` is a prompt, not an error: a message can legitimately
/// start with a path.
fn parse_command(line: &str) -> Option<Command<'_>> {
    let remainder = line.strip_prefix('/')?;
    let (name, argument) = match remainder.split_once(char::is_whitespace) {
        Some((name, argument)) => (name, argument.trim()),
        None => (remainder, ""),
    };
    let argument = (!argument.is_empty()).then_some(argument);
    match name {
        "help" if argument.is_none() => Some(Command::Help),
        "clear" if argument.is_none() => Some(Command::Clear),
        "reload" if argument.is_none() => Some(Command::Reload),
        "model" => Some(Command::Model(argument)),
        "cc" => Some(Command::Claude {
            mode: AskMode::Critique,
            text: argument,
        }),
        "ca" => Some(Command::Claude {
            mode: AskMode::Ask,
            text: argument,
        }),
        "cn" => Some(Command::Claude {
            mode: AskMode::Standalone,
            text: argument,
        }),
        "ag" => Some(Command::Antigravity {
            mode: AskMode::Critique,
            text: argument,
        }),
        "aga" => Some(Command::Antigravity {
            mode: AskMode::Ask,
            text: argument,
        }),
        "agn" => Some(Command::Antigravity {
            mode: AskMode::Standalone,
            text: argument,
        }),
        _ => None,
    }
}

struct ParsedInput<'a> {
    prompt: Option<&'a str>,
    command: Option<Command<'a>>,
}

fn model_prompt<'a>(
    command: Option<&Command<'a>>,
    prompt: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    match (command, prompt) {
        (Some(Command::Model(Some(alias))), Some(prompt)) => Some((alias, prompt)),
        _ => None,
    }
}

/// A prefixed command uses `;` as an explicit boundary:
/// `/clear; explain this` and `/model flash; explain this`.
fn parse_input(input: &str) -> ParsedInput<'_> {
    let trimmed = input.trim();
    // `!` claims the whole line, so it is checked before anything else: no
    // prompt can start with one and no `/` form can follow one.
    if let Some(command) = parse_bash(trimmed) {
        return ParsedInput {
            prompt: None,
            command: Some(Command::Bash(command)),
        };
    }
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
            let (alias, prompt) = remainder.split_once(';')?;
            let alias = alias.trim();
            let prompt = prompt.trim();
            (!alias.is_empty() && !prompt.is_empty())
                .then_some((prompt, Command::Model(Some(alias))))
        })
}

fn prompt_after_separator(remainder: &str) -> Option<&str> {
    remainder.strip_prefix(';').and_then(nonempty_trimmed)
}

fn nonempty_trimmed(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn executable_available(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&path).any(|directory| {
        let executable = directory.join(name);
        if cfg!(windows) {
            executable.with_extension("exe").is_file() || executable.is_file()
        } else {
            executable.is_file()
        }
    })
}

fn help_commands(has_claude: bool, has_antigravity: bool) -> String {
    let mut commands = String::from(
        "  /help               Show this help\n  /model [alias]      Choose a model or switch directly to an alias\n  /reload             Reload configuration\n  /clear              Reset context and start a fresh session\n",
    );
    if has_claude {
        commands.push_str(
            "  /cc [focus]         Ask Claude to critique the discussion\n  /ca <question>      Ask Claude about the discussion (or request implementation)\n  /cn <prompt>        Ask Claude without discussion history\n",
        );
    }
    if has_antigravity {
        commands.push_str(
            "  /ag [focus]         Ask Antigravity to critique the discussion\n  /aga <question>     Ask Antigravity about the discussion (or request implementation)\n  /agn <prompt>       Ask Antigravity without discussion history\n",
        );
    }
    commands.push_str(
        "\n  ! <command>         Run a bash command, show it, and record it for the model\n  !                   Show the recent bash commands\n\n  /clear; <prompt>          Reset context, then send a prompt\n  /model <alias>; <prompt>  Switch model, then send a prompt\n\n  Ctrl+C              Stop a running request, a running ! command, or discard the current input\n\nUnknown slash commands are sent as ordinary prompts to the current model. A ! command\nnever sends a prompt: the model reads its output when you next write to it.",
    );
    commands
}

fn print_help(agent: &Agent, has_claude: bool, has_antigravity: bool) {
    output::head("Current session");
    eprintln!("{}\n", agent.summary());
    output::head("Commands");
    eprintln!("{}", help_commands(has_claude, has_antigravity));
}

/// Returns false when the command failed or selection was cancelled, so a
/// prompt attached to it is never sent with unintended state.
async fn run_command(
    agent: &mut Agent,
    history: &Rc<RefCell<BashHistory>>,
    command: Command<'_>,
) -> bool {
    match command {
        Command::Help => {
            print_help(
                agent,
                executable_available("claude"),
                executable_available("agy"),
            );
            true
        }
        Command::Clear => match agent.clear_session() {
            Ok(()) => true,
            Err(e) => {
                output::fail(&format!("{e:#}"));
                false
            }
        },
        Command::Reload => match agent.reload_models() {
            Ok(()) => {
                output::ok("configuration reloaded");
                output::step(&format!("model: {}", agent.models().active()));
                true
            }
            Err(e) => {
                output::fail(&format!("configuration reload failed: {e:#}"));
                output::note(&format!("continuing with {}", agent.models().active()));
                false
            }
        },
        Command::Model(alias) => switch_model(agent, alias).await,
        Command::Bash(command) => run_bash(agent, history, command).await,
        Command::Claude { mode, text } => {
            use crate::claude::{DEFAULT_INSTRUCTION, Request};

            let focused;
            let request = match (mode, text) {
                (AskMode::Critique, None) => Request::Discussion(DEFAULT_INSTRUCTION),
                (AskMode::Critique, Some(focus)) => {
                    focused = format!("{DEFAULT_INSTRUCTION} Focus on: {focus}");
                    Request::Discussion(&focused)
                }
                (AskMode::Ask, Some(question)) => Request::Discussion(question),
                (AskMode::Standalone, Some(prompt)) => Request::Direct(prompt),
                (mode, None) => {
                    output::fail(&format!(
                        "usage: {} <question or prompt>",
                        mode.as_command()
                    ));
                    return false;
                }
            };
            output::step(if matches!(request, Request::Direct(_)) {
                "asking Claude (sending your prompt only)..."
            } else {
                "asking Claude (sharing discussion and recorded tool calls/results)..."
            });

            match crate::claude::review(agent.messages(), request, agent.cwd()).await {
                Ok(review) => {
                    output::head("Claude response");
                    println!("{}", review.text);
                    if let Err(error) = agent.record_reference(review.reference) {
                        output::fail(&format!(
                            "Claude response displayed but could not be saved: {error:#}"
                        ));
                        return false;
                    }
                    output::note(
                        "Claude response saved as reference — tell the current model how to proceed",
                    );
                    true
                }
                Err(error) => {
                    output::fail(&format!("{error:#}"));
                    false
                }
            }
        }
        Command::Antigravity { mode, text } => {
            use crate::antigravity::{DEFAULT_INSTRUCTION, Request};

            let focused;
            let request = match (mode, text) {
                (AskMode::Critique, None) => Request::Discussion(DEFAULT_INSTRUCTION),
                (AskMode::Critique, Some(focus)) => {
                    focused = format!("{DEFAULT_INSTRUCTION} Focus on: {focus}");
                    Request::Discussion(&focused)
                }
                (AskMode::Ask, Some(question)) => Request::Discussion(question),
                (AskMode::Standalone, Some(prompt)) => Request::Direct(prompt),
                (mode, None) => {
                    output::fail(&format!(
                        "usage: {} <question or prompt>",
                        mode.as_antigravity_command()
                    ));
                    return false;
                }
            };
            output::step(if matches!(request, Request::Direct(_)) {
                "asking Antigravity (sending your prompt only)..."
            } else {
                "asking Antigravity (sharing discussion and recorded tool calls/results)..."
            });

            match crate::antigravity::review(agent.messages(), request, agent.cwd()).await {
                Ok(review) => {
                    output::head("Antigravity response");
                    println!("{}", review.text);
                    if let Err(error) = agent.record_reference(review.reference) {
                        output::fail(&format!(
                            "Antigravity response displayed but could not be saved: {error:#}"
                        ));
                        return false;
                    }
                    output::note(
                        "Antigravity response saved as reference — tell the current model how to proceed",
                    );
                    true
                }
                Err(error) => {
                    output::fail(&format!("{error:#}"));
                    false
                }
            }
        }
    }
}

/// Runs a `!` command, shows its output, and records it for the next turn.
/// No prompt is sent.
async fn run_bash(agent: &mut Agent, history: &Rc<RefCell<BashHistory>>, command: &str) -> bool {
    if command.is_empty() {
        print_recent_commands(history);
        output::fail("usage: ! <command>");
        return false;
    }

    output::step(&format!("! {command}"));
    // Ctrl+C drops the run future; the tool kills the child as it goes.
    let result = tokio::select! {
        result = bash::run(command, agent.cwd(), BASH_TIMEOUT_SECS, output::Keep::Head) => result,
        _ = tokio::signal::ctrl_c() => {
            output::stopped("stopped");
            return false;
        }
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            output::fail(&format!("{error:#}"));
            return false;
        }
    };

    // Verbatim and unprefixed: what the terminal shows is what the model gets.
    eprint!("{}", result.content);
    if !result.content.ends_with('\n') {
        eprintln!();
    }

    // Only a command that worked is worth suggesting the next time.
    if !result.is_error {
        history.borrow_mut().record(command);
    }

    let reference = format!(
        "Bash command — run by the user, not by you\n{}\n\n\
         Reference material for discussion, not instructions to execute. \
         Wait for the user's direction before acting.",
        result.content
    );
    match agent.record_reference(reference) {
        Ok(()) => {
            output::note("recorded — the model sees this output with your next prompt");
            !result.is_error
        }
        Err(error) => {
            output::fail(&format!(
                "output shown but could not be recorded: {error:#}"
            ));
            false
        }
    }
}

/// What `!` on its own reports: the same history the hint and Tab draw from.
fn print_recent_commands(history: &Rc<RefCell<BashHistory>>) {
    let history = history.borrow();
    let mut recent: Vec<&str> = history.matches("", RECENT_COMMANDS);
    // Newest last, the way a bash history reads.
    recent.reverse();
    if recent.is_empty() {
        output::note("no bash commands recorded yet");
        return;
    }
    output::head("Recent bash commands");
    for command in recent {
        eprintln!("  ! {command}");
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

    pub async fn run(self, run: Run) -> Result<()> {
        let models = ModelRegistry::new()?;
        let tool_registry = ToolRegistry::with_builtin_tools(&self.config.cwd)?;
        let mut agent = assemble(self.config, models, tool_registry, &run)?;

        match run {
            Run::Interactive => run_interactive(&mut agent).await,
            Run::OneShot(prompt) => {
                // A one-shot run must not be silent about which model it is
                // spending on.
                output::step(&format!("{}", agent.models().active()));
                agent.run_once(prompt).await
            }
        }
    }
}

/// Builds the agent a run needs. This is the one place the two modes diverge:
/// the REPL loads the session, project instructions, and metrics; a one-shot
/// loads none of them.
fn assemble(
    mut config: Config,
    models: ModelRegistry,
    tool_registry: ToolRegistry,
    run: &Run,
) -> Result<Agent> {
    match run {
        Run::Interactive => {
            let instructions = prompt::instructions(&config.cwd);
            config.prompt_sources = instructions.iter().map(|i| i.name).collect();
            config.system_prompt = Some(prompt::system_prompt(
                &config.cwd,
                &tool_registry.names(),
                &instructions,
            ));
            Agent::new(config, models, tool_registry)
        }
        Run::OneShot(_) => {
            config.prompt_sources = Vec::new();
            config.system_prompt = Some(prompt::system_prompt(
                &config.cwd,
                &tool_registry.names(),
                &[],
            ));
            Ok(Agent::ephemeral(config, models, tool_registry))
        }
    }
}

/// The interactive REPL: read a line, run it, repeat until input closes.
async fn run_interactive(agent: &mut Agent) -> Result<()> {
    // The banner is OneLoop talking about itself, so it goes where the rest
    // of that goes — stdout stays the model's.
    eprintln!("OneLoop");
    eprintln!("{}", agent.summary());
    eprintln!();
    eprintln!("Type /help for commands.");
    eprintln!();

    // The user's own `!` commands, shared with the helper so Tab and the hint
    // see each command as soon as it succeeds.
    let history = Rc::new(RefCell::new(BashHistory::open()));

    // Canonical mode silently drops input past the tty's 4096-byte line
    // buffer and locks up the prompt on long pastes.
    let mut editor = Editor::<ReplHelper, DefaultHistory>::new()?;
    editor.set_helper(Some(ReplHelper {
        history: Rc::clone(&history),
    }));

    loop {
        // Re-read per turn: `/clear` empties the session and `/model`
        // switches windows, so the gauge is fresh every time it is shown.
        let gauge = agent.remaining_context();
        let prompt = interactive_prompt(&agent.models().active().alias, gauge);
        let line = match editor.readline(&prompt) {
            Ok(input) => input.trim().to_string(),
            // Ctrl+C at the prompt discards the current draft.
            Err(ReadlineError::Interrupted) => continue,
            // A closed input stream still exits; do not spin on EOF.
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(&line);

        let ParsedInput { prompt, command } = parse_input(&line);
        if let Some((alias, prompt)) = model_prompt(command.as_ref(), prompt) {
            run_interactive_turn(agent, prompt, Some(alias)).await;
            eprintln!();
            continue;
        }
        if let Some(command) = command
            && !run_command(agent, &history, command).await
        {
            eprintln!();
            continue;
        }
        let Some(prompt) = prompt else {
            eprintln!();
            continue;
        };

        run_interactive_turn(agent, prompt, None).await;
        eprintln!();
    }

    Ok(())
}

/// Errors are reported, never propagated — a failed turn must not end the
/// REPL.
async fn run_interactive_turn(agent: &mut Agent, line: &str, model: Option<&str>) {
    // Ctrl+C drops the run future mid-flight.
    let mut interrupted = false;
    tokio::select! {
        result = agent.run_once_with_model(line.to_string(), model) => {
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
    use super::{
        AskMode, Command, ReplHelper, bash_fragment, interactive_prompt, parse_command, parse_input,
    };
    use crate::bash_history::BashHistory;
    use rustyline::Context;
    use rustyline::completion::Completer;
    use rustyline::hint::Hinter;
    use rustyline::history::DefaultHistory;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The helper as the REPL holds it, over a history with no file behind it.
    fn helper(commands: &[&str]) -> ReplHelper {
        ReplHelper {
            history: Rc::new(RefCell::new(BashHistory::for_test(commands))),
        }
    }

    #[test]
    fn ag_modes_report_their_command_names() {
        assert_eq!(AskMode::Critique.as_antigravity_command(), "/ag");
        assert_eq!(AskMode::Ask.as_antigravity_command(), "/aga");
        assert_eq!(AskMode::Standalone.as_antigravity_command(), "/agn");
    }

    #[test]
    fn bare_ag_is_the_default_critique_command() {
        assert!(matches!(
            parse_input("/ag").command,
            Some(Command::Antigravity {
                mode: AskMode::Critique,
                text: None
            })
        ));
    }

    #[test]
    fn aga_and_agn_parse_their_arguments() {
        assert!(matches!(
            parse_input("/aga question").command,
            Some(Command::Antigravity {
                mode: AskMode::Ask,
                text: Some("question")
            })
        ));
        assert!(matches!(
            parse_input("/agn prompt").command,
            Some(Command::Antigravity {
                mode: AskMode::Standalone,
                text: Some("prompt")
            })
        ));
    }

    #[test]
    fn interactive_prompt_names_the_active_model() {
        assert_eq!(interactive_prompt("qwen", None), "(qwen)> ");
    }

    #[test]
    fn interactive_prompt_shows_the_context_left() {
        assert_eq!(interactive_prompt("qwen", Some(80)), "(qwen ~ 80%)> ");
    }

    #[test]
    fn cc_without_arguments_is_critique() {
        assert!(matches!(
            parse_input("/cc").command,
            Some(Command::Claude {
                mode: AskMode::Critique,
                text: None
            })
        ));
    }

    #[test]
    fn cc_with_focus_text_is_critique_with_focus() {
        let parsed = parse_input("/cc the retry logic");
        assert!(matches!(
            (parsed.command, parsed.prompt),
            (
                Some(Command::Claude {
                    mode: AskMode::Critique,
                    text: Some("the retry logic")
                }),
                None
            )
        ));
    }

    #[test]
    fn ca_requires_a_question() {
        assert!(matches!(
            parse_input("/ca").command,
            Some(Command::Claude {
                mode: AskMode::Ask,
                text: None
            })
        ));
    }

    #[test]
    fn ca_with_question_is_ask_with_discussion() {
        let parsed = parse_input("/ca What about the retry logic?");
        assert!(matches!(
            (parsed.command, parsed.prompt),
            (
                Some(Command::Claude {
                    mode: AskMode::Ask,
                    text: Some("What about the retry logic?")
                }),
                None
            )
        ));
    }

    #[test]
    fn cn_requires_a_prompt() {
        assert!(matches!(
            parse_input("/cn").command,
            Some(Command::Claude {
                mode: AskMode::Standalone,
                text: None
            })
        ));
    }

    #[test]
    fn cn_with_prompt_is_standalone() {
        let parsed = parse_input("/cn Hello world");
        assert!(matches!(
            (parsed.command, parsed.prompt),
            (
                Some(Command::Claude {
                    mode: AskMode::Standalone,
                    text: Some("Hello world")
                }),
                None
            )
        ));
    }

    #[test]
    fn help_is_handled_without_a_model_prompt() {
        let parsed = parse_input(" /help ");
        assert!(matches!(
            (parsed.command, parsed.prompt),
            (Some(Command::Help), None)
        ));
    }

    #[test]
    fn nonexact_help_commands_remain_prompts() {
        for input in ["/helpful", "/help now", "/help.", "//help"] {
            let parsed = parse_input(input);
            assert!(
                matches!((parsed.command, parsed.prompt), (None, Some(prompt)) if prompt == input)
            );
        }
    }

    #[test]
    fn clear_is_a_command() {
        assert!(matches!(parse_command("/clear"), Some(Command::Clear)));
    }

    #[test]
    fn reload_is_a_command_without_arguments() {
        assert!(matches!(parse_command("/reload"), Some(Command::Reload)));
        assert!(parse_command("/reload now").is_none());
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
        let parsed = parse_input("/clear; Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("Explain this"), Some(Command::Clear))
        ));
    }

    #[test]
    fn prefixed_model_is_extracted() {
        let parsed = parse_input("/model flash; Why does this fail?");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (
                Some("Why does this fail?"),
                Some(Command::Model(Some("flash")))
            )
        ));
    }

    /// An alias may carry a dot of its own, so the separator cannot be one.
    /// Splitting on `.` cut `deepseek-v4.1-flash` down to `deepseek-v4`.
    #[test]
    fn a_dotted_alias_survives_the_separator() {
        let parsed = parse_input("/model deepseek-v4.1-flash; explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (
                Some("explain this"),
                Some(Command::Model(Some("deepseek-v4.1-flash")))
            )
        ));
    }

    #[test]
    fn spoken_clear_stays_in_the_prompt() {
        let parsed = parse_input("slash clear semicolon Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("slash clear semicolon Explain this"), None)
        ));
    }

    #[test]
    fn spoken_model_stays_in_the_prompt() {
        let parsed = parse_input("slash model flash semicolon Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("slash model flash semicolon Explain this"), None)
        ));
    }

    #[test]
    fn typed_command_with_spoken_separator_stays_in_the_prompt() {
        let parsed = parse_input("/clear semicolon Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/clear semicolon Explain this"), None)
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
    fn a_dot_is_no_longer_a_separator() {
        // The form the old separator produced: nothing before the dot was a
        // command, so this is an ordinary prompt.
        let parsed = parse_input("/clear. Explain this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/clear. Explain this"), None)
        ));
    }

    #[test]
    fn unknown_prefixed_slash_word_stays_in_the_prompt() {
        let parsed = parse_input("/etc/hosts; Read this");

        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/etc/hosts; Read this"), None)
        ));
    }

    #[test]
    fn retired_claude_command_passes_through_as_prompt() {
        let parsed = parse_input("/claude");
        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/claude"), None)
        ));
    }

    #[test]
    fn retired_claude_with_text_passes_through_as_prompt() {
        let parsed = parse_input("/claude hello");
        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/claude hello"), None)
        ));
    }

    #[test]
    fn typo_cc_command_passes_through_as_prompt() {
        let parsed = parse_input("/cx hello");
        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/cx hello"), None)
        ));
    }

    #[test]
    fn only_exact_claude_command_names_are_recognized() {
        for input in ["//cc", "/CC", "/cca focus", "/ca. foo", "/cn."] {
            let parsed = parse_input(input);
            assert!(
                matches!((parsed.prompt, parsed.command), (Some(prompt), None) if prompt == input)
            );
        }
    }

    #[test]
    fn whitespace_only_claude_arguments_are_missing() {
        for input in ["/ca   ", "/cn\t"] {
            let parsed = parse_input(input);
            assert!(matches!(
                (parsed.command, parsed.prompt),
                (Some(Command::Claude { text: None, .. }), None)
            ));
        }
    }

    #[test]
    fn cc_dot_passes_through_as_prompt() {
        let parsed = parse_input("/cc.");
        assert!(matches!(
            (parsed.prompt, parsed.command),
            (Some("/cc."), None)
        ));
    }

    #[test]
    fn a_bang_line_is_a_bash_command() {
        assert!(matches!(
            parse_input("!ls -la").command,
            Some(Command::Bash("ls -la"))
        ));
    }

    #[test]
    fn a_space_after_the_bang_is_optional() {
        assert!(matches!(
            parse_input("! ls -la").command,
            Some(Command::Bash("ls -la"))
        ));
    }

    #[test]
    fn a_bang_command_keeps_its_semicolons() {
        assert!(matches!(
            parse_input("!cargo test; echo done").command,
            Some(Command::Bash("cargo test; echo done"))
        ));
    }

    #[test]
    fn a_bang_command_is_never_a_prompt() {
        let parsed = parse_input("!echo hi");
        assert!(parsed.prompt.is_none());
    }

    #[test]
    fn a_bare_bang_is_a_bash_command_with_nothing_to_run() {
        assert!(matches!(
            (parse_input("!").prompt, parse_input("!").command),
            (None, Some(Command::Bash("")))
        ));
    }

    #[test]
    fn a_completion_starts_where_the_command_does() {
        assert_eq!(bash_fragment("!git lo", 7), Some((1, "git lo")));
        assert_eq!(bash_fragment("! git lo", 8), Some((2, "git lo")));
        assert_eq!(bash_fragment("!/bin/ls", 3), Some((1, "/b")));
    }

    #[test]
    fn a_line_that_is_not_a_bash_command_offers_no_completion() {
        assert_eq!(bash_fragment("git lo", 6), None);
        assert_eq!(bash_fragment("/model", 6), None);
    }

    #[test]
    fn a_bang_prefix_is_hinted_from_the_user_bash_history() {
        let helper = helper(&["git status --short"]);
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        assert_eq!(
            helper.hint("!git st", 7, &context),
            Some("atus --short".to_string())
        );
    }

    #[test]
    fn a_hint_stops_at_the_end_of_the_line() {
        let helper = helper(&["git status --short"]);
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        assert_eq!(helper.hint("!git st", 4, &context), None);
    }

    #[test]
    fn tab_replaces_the_command_after_the_sigil() {
        let helper = helper(&["git status --short"]);
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, candidates) = helper.complete("!git st", 7, &context).unwrap();
        assert_eq!(start, 1);
        assert_eq!(candidates[0].replacement, "git status --short");
    }
}
