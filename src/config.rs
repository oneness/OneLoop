use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    /// Populated by `App::run` via `build_system_prompt` once the tool
    /// registry exists — the preamble needs the actual tool names.
    pub system_prompt: Option<String>,
    pub prompt_sources: Vec<&'static str>,
}

impl Default for Config {
    fn default() -> Self {
        let cwd = env::var("ONELOOP_ORIGINAL_DIR")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())
            .unwrap_or_else(|_| PathBuf::from("."));
        // Overwritten in `App::run` once the tool registry exists: which
        // tools registered decides part of both.
        let prompt_sources = prompt_sources(&cwd);
        Self {
            cwd,
            system_prompt: None,
            prompt_sources,
        }
    }
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

pub fn build_system_prompt(cwd: &Path, tool_names: &[&str]) -> String {
    let sections: Vec<String> = [
        Some(environment_block(cwd)),
        load_file(cwd.join("AGENTS.md").as_path()),
        load_file(cwd.join(".oneloop").join("memory.md").as_path())
            .map(|m| format!("## Memory\n\n{m}")),
    ]
    .into_iter()
    .flatten()
    .collect();

    format!("{}\n\n{}", tool_preamble(tool_names), sections.join("\n\n"))
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

/// Cap on agent-loop iterations per prompt.
pub const DEFAULT_MAX_ITERATIONS: usize = 50;

/// Read an env var, falling back to `default` when unset or unparsable.
pub fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn memory_path(cwd: &Path) -> PathBuf {
    cwd.join(".oneloop").join("memory.md")
}

pub fn prompt_sources(cwd: &Path) -> Vec<&'static str> {
    let mut sources = Vec::new();
    if load_file(cwd.join("AGENTS.md").as_path()).is_some() {
        sources.push("AGENTS.md");
    }
    if load_file(memory_path(cwd).as_path()).is_some() {
        sources.push(".oneloop/memory.md");
    }
    sources
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-config-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn system_prompt_starts_with_tool_preamble() {
        let dir = temp_dir("preamble");
        fs::write(dir.join("AGENTS.md"), "Use `semble search` to find code.").unwrap();

        let prompt = build_system_prompt(&dir, &["read", "bash"]);

        assert!(prompt.starts_with("Your only tools are: read, bash"));
        assert!(prompt.contains("semble search"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_always_carries_environment() {
        let dir = temp_dir("empty");

        let prompt = build_system_prompt(&dir, &["read"]);

        assert!(prompt.contains("## Environment"));
        assert!(prompt.contains(&dir.display().to_string()));
        let _ = fs::remove_dir_all(&dir);
    }
}
