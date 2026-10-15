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
/// plus whatever instructions it was given.
pub fn system_prompt(cwd: &Path, tool_names: &[&str], instructions: &[Instruction]) -> String {
    let mut sections = vec![environment_block(cwd)];
    sections.extend(
        instructions
            .iter()
            .map(|instruction| instruction.body.clone()),
    );
    format!("{}\n\n{}", tool_preamble(tool_names), sections.join("\n\n"))
}

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
    fn a_one_shot_prompt_carries_no_instructions() {
        let dir = temp_dir("oneshot");
        fs::write(dir.join("AGENTS.md"), "Use `semble search` to find code.").unwrap();

        let prompt = system_prompt(&dir, &["read"], &[]);

        assert!(!prompt.contains("semble search"));
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
