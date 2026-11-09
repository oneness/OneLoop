//! The system prompt: what the model is told before the conversation.
//!
//! Kept apart from `config` so the one place that decides which instruction
//! files a run loads is also the one place that names them.

use std::{env, fs, path::Path};

/// A project instruction file folded into the system prompt. Named so the
/// prompt and the `/help` summary read the same list and cannot drift.
pub struct Instruction {
    pub name: &'static str,
    pub body: String,
}

/// The instruction files this project loads, in order. Empty when there are
/// none — which is what a one-shot passes to `system_prompt`.
pub fn instructions(cwd: &Path) -> Vec<Instruction> {
    [load_instruction("AGENTS.md", cwd)]
        .into_iter()
        .flatten()
        .collect()
}

/// The system prompt for a run: what the model needs to call tools correctly,
/// then the standards that hold everywhere, then — for an agent that has one —
/// what this particular agent is for, then whatever instructions it was given.
///
/// `persona` is `None` for the default agent, and `None` has to reproduce the
/// prompt byte for byte: the coding agent's behavior is not allowed to move
/// because a slot appeared beside it.
pub fn system_prompt(
    cwd: &Path,
    tool_names: &[&str],
    instructions: &[Instruction],
    persona: Option<&str>,
) -> String {
    let environment = environment_block(cwd);
    let mut sections = vec![engineering_block(), environment.as_str()];
    sections.extend(persona);
    sections.extend(
        instructions
            .iter()
            .map(|instruction| instruction.body.as_str()),
    );
    format!("{}\n\n{}", tool_preamble(tool_names), sections.join("\n\n"))
}

/// The standards that hold in any repository. A checkout without an `AGENTS.md`
/// loads nothing else, so these are the only rules that travel with the binary.
///
/// They are deliberately the ones no check can enforce. Anything a lint, a test
/// or a type can catch belongs there instead — that is what keeps this list
/// short enough to be read rather than skimmed.
fn engineering_block() -> &'static str {
    "\
## Engineering Tenets

They hold on every task; where no check can, they decide.

- **Choose correct over simple, and simple over fast.** Where they conflict,
  that order holds.
- **Look it up; don't recall it.** Use tools to read the API, the path, and the
  behavior. Memory is the last resort — say so when you fall back on it.
- **Show your work.** Verify before you claim success, and explain why rather
  than what.
- **Do each thing one way.** Take the smallest change that works; don't add a
  second mechanism beside the first.
- **Don't break what you don't own.** Understand existing code before changing
  it, and never delete or overwrite work you cannot recover without asking."
}

/// What a reviewer is for. A domain agent's words live here, beside the tenets,
/// for the same reason the tenets do: a prompt written inside a preset is a
/// prompt nobody looks at twice.
///
/// It states the tool set as a limit rather than a preference — this agent has
/// no write tool, so a model that promises to fix what it found has promised
/// something it cannot do.
pub const REVIEWER_PERSONA: &str = "\
## Reviewing

You review code; you do not change it. Read the diff and the files it touches,
follow what they depend on, and run the check that settles a question rather
than reasoning from the shape of the code.

Report what is wrong and what it would take to fix, most serious first, naming
the file and line for each finding. Say plainly when you find nothing worth
changing.

You have no tool that writes, edits or applies a patch, and your shell must not
stand in for one: `sed -i`, `git checkout` and a redirect all change the tree you
are asked to judge, and a review that changes the code under review is not a
review. If something cannot be settled by reading, or by a command that only
reads, say that you could not settle it.";

fn load_instruction(name: &'static str, cwd: &Path) -> Option<Instruction> {
    Some(Instruction {
        name,
        body: load_file(&cwd.join(name))?,
    })
}

/// Runtime facts the model cannot observe on its own. Without them it invents
/// a home directory — `/Users/runner`, straight out of CI logs in training
/// data — and hands the read tool an absolute path that never existed here.
fn environment_block(cwd: &Path) -> String {
    let home = env::var("HOME").unwrap_or_else(|_| "<unknown>".to_string());
    format!(
        "## Environment\n\n\
         - Working directory: {}\n\
         - Home directory: {home}\n\
         - Platform: {}\n\n\
         A path beginning with `/` or `~` is already anchored: pass it to a \
         tool exactly as written, and never prefix the working directory onto \
         it. Only a path with no such prefix is relative, and those resolve \
         against the working directory. `~` is expanded for you. Never invent \
         an absolute path from any root other than the two above — the \
         working directory is often not the home directory.",
        cwd.display(),
        env::consts::OS,
    )
}

/// Inoculate loaded project instructions: AGENTS.md files are often written
/// for other agents and name CLI helpers in ways models mistake for tools.
fn tool_preamble(tool_names: &[&str]) -> String {
    format!(
        "Your only tools are: {}. Any other command or capability mentioned \
         in the instructions below is a CLI program — run it with the bash \
         tool, never as a tool call.",
        tool_names.join(", ")
    )
}

fn load_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        None
    } else {
        Some(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-prompt-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn system_prompt_starts_with_tool_preamble() {
        let dir = temp_dir("preamble");
        fs::write(dir.join("AGENTS.md"), "Use `semble search` to find code.").unwrap();

        let prompt = system_prompt(&dir, &["read", "bash"], &instructions(&dir), None);

        assert!(prompt.starts_with("Your only tools are: read, bash"));
        assert!(prompt.contains("semble search"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_always_carries_environment() {
        let dir = temp_dir("empty");

        let prompt = system_prompt(&dir, &["read"], &[], None);

        assert!(prompt.contains("## Environment"));
        assert!(prompt.contains(&dir.display().to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_always_carries_the_engineering_tenets() {
        let dir = temp_dir("tenets");

        let prompt = system_prompt(&dir, &["read"], &[], None);

        assert!(prompt.contains("## Engineering Tenets"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engineering_tenets_precede_the_environment() {
        let dir = temp_dir("order");

        let prompt = system_prompt(&dir, &["read"], &[], None);

        let tenets = prompt.find("## Engineering Tenets").unwrap();
        let environment = prompt.find("## Environment").unwrap();
        assert!(tenets < environment);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_one_shot_prompt_carries_no_instructions() {
        let dir = temp_dir("oneshot");
        fs::write(dir.join("AGENTS.md"), "Use `semble search` to find code.").unwrap();

        let prompt = system_prompt(&dir, &["read"], &[], None);

        assert!(!prompt.contains("semble search"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A persona is an agent's own words, and they sit after the standards that
    /// hold everywhere and before the project's instructions — so a project can
    /// still say what it needs without the agent's purpose being buried.
    #[test]
    fn a_persona_sits_between_the_tenets_and_the_instructions() {
        let dir = temp_dir("persona-order");
        fs::write(dir.join("AGENTS.md"), "project rules").unwrap();

        let prompt = system_prompt(
            &dir,
            &["read"],
            &instructions(&dir),
            Some("## Reviewing\n\nRead the diff."),
        );

        let tenets = prompt.find("## Engineering Tenets").unwrap();
        let persona = prompt.find("## Reviewing").unwrap();
        let project = prompt.find("project rules").unwrap();
        assert!(tenets < persona && persona < project, "{prompt}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The default agent has no persona, and having the slot must not change a
    /// single byte of what it is told: every other test in this module, and the
    /// model, read the same prompt as before the slot existed.
    #[test]
    fn no_persona_is_the_prompt_without_the_slot() {
        let dir = temp_dir("no-persona");

        let prompt = system_prompt(&dir, &["read"], &[], None);

        let expected = format!(
            "{}\n\n{}\n\n{}",
            tool_preamble(&["read"]),
            engineering_block(),
            environment_block(&dir)
        );
        assert_eq!(prompt, expected);
        // No section at all, rather than an empty one: a `Some`-shaped slot
        // that renders as nothing would leave a doubled blank line behind.
        assert!(!prompt.contains("\n\n\n"), "{prompt}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn instructions_loads_agents_md_when_present() {
        let dir = temp_dir("names");
        assert!(instructions(&dir).is_empty());
        fs::write(dir.join("AGENTS.md"), "x").unwrap();
        assert_eq!(instructions(&dir).len(), 1);
        assert_eq!(instructions(&dir)[0].name, "AGENTS.md");
        let _ = fs::remove_dir_all(&dir);
    }
}
