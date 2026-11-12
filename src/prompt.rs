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
/// then the standards that hold everywhere, then whatever instructions the
/// project was given.
pub fn system_prompt(cwd: &Path, tool_names: &[&str], instructions: &[Instruction]) -> String {
    let environment = environment_block(cwd);
    let mut sections = vec![engineering_block(), environment.as_str()];
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

/// What a conversation is compressed into, for the agent that continues it.
///
/// This is a whole system prompt, not a section of the coding agent's: a
/// summarizer is not an agent with tenets and an environment to obey, it is a
/// transformation, and everything it is told is about the transformation. It
/// says the tool set is nonexistent rather than limited — there is no registry
/// behind this prompt at all — so a model that decides it should go and read
/// the repository has been told, in the only place that can tell it, that it
/// cannot.
pub const SUMMARIZER: &str = "\
You compress a conversation between a user and a coding agent into a briefing
for an agent that will continue the work with none of the history.

Write it in this order, and stop:

## What we are working on
## Decisions, and why
## What remains
## Open questions

- Name files and symbols exactly as the conversation did. Never invent a path.
- Say what was tried and failed. A repetition is worse than a gap.
- Leave out what was read and not concluded: a tool result that changed nothing
  is not part of the state.
- Prefer short lines to paragraphs. The whole briefing is one screen.
- If the conversation disagrees with itself, say so rather than picking a side.

You have no tools and no access to the repository. If the conversation does not
contain something, it is not there — say it is unknown rather than guessing.";

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

        let prompt = system_prompt(&dir, &["read", "bash"], &instructions(&dir));

        assert!(prompt.starts_with("Your only tools are: read, bash"));
        assert!(prompt.contains("semble search"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_always_carries_environment() {
        let dir = temp_dir("empty");

        let prompt = system_prompt(&dir, &["read"], &[]);

        assert!(prompt.contains("## Environment"));
        assert!(prompt.contains(&dir.display().to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_always_carries_the_engineering_tenets() {
        let dir = temp_dir("tenets");

        let prompt = system_prompt(&dir, &["read"], &[]);

        assert!(prompt.contains("## Engineering Tenets"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engineering_tenets_precede_the_environment() {
        let dir = temp_dir("order");

        let prompt = system_prompt(&dir, &["read"], &[]);

        let tenets = prompt.find("## Engineering Tenets").unwrap();
        let environment = prompt.find("## Environment").unwrap();
        assert!(tenets < environment);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_one_shot_prompt_carries_no_instructions() {
        let dir = temp_dir("oneshot");
        fs::write(dir.join("AGENTS.md"), "Use `semble search` to find code.").unwrap();

        let prompt = system_prompt(&dir, &["read"], &[]);

        assert!(!prompt.contains("semble search"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Three sections and two separators: a prompt with an emptied slot would
    /// carry a doubled blank line where nothing was written.
    #[test]
    fn the_prompt_has_no_emptied_section() {
        let dir = temp_dir("no-slot");

        let prompt = system_prompt(&dir, &["read"], &[]);

        let expected = format!(
            "{}\n\n{}\n\n{}",
            tool_preamble(&["read"]),
            engineering_block(),
            environment_block(&dir)
        );
        assert_eq!(prompt, expected);
        assert!(!prompt.contains("\n\n\n"), "{prompt}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The summarizer is handed text and nothing else, so the prompt has to say
    /// so: a model that believes it can go and check the repository will invent
    /// what it finds there.
    #[test]
    fn the_summarizer_is_told_it_cannot_reach_the_repository() {
        assert!(SUMMARIZER.contains("no tools"), "{SUMMARIZER}");
        assert!(
            SUMMARIZER.contains("no access to the repository"),
            "{SUMMARIZER}"
        );
    }

    /// It produces a briefing, not a review and not a reply: the four headings
    /// are what the agent that continues the work reads first.
    #[test]
    fn the_summarizer_asks_for_the_four_sections() {
        for section in [
            "## What we are working on",
            "## Decisions, and why",
            "## What remains",
            "## Open questions",
        ] {
            assert!(SUMMARIZER.contains(section), "missing {section}");
        }
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
