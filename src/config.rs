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
        let prompt_sources = prompt_sources(&cwd);
        Self {
            cwd,
            system_prompt: None,
            prompt_sources,
        }
    }
}

pub fn build_system_prompt(cwd: &Path, tool_names: &[&str]) -> Option<String> {
    let agents = load_file(cwd.join("AGENTS.md").as_path());
    let memory = load_file(cwd.join(".oneloop").join("memory.md").as_path())
        .map(|m| format!("## Memory\n\n{m}"));

    let body = match (agents, memory) {
        (None, None) => return None,
        (Some(a), None) => a,
        (None, Some(m)) => m,
        (Some(a), Some(m)) => format!("{a}\n\n{m}"),
    };
    Some(format!("{}\n\n{body}", tool_preamble(tool_names)))
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
        let dir = std::env::temp_dir().join(format!(
            "oneloop-config-test-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn system_prompt_starts_with_tool_preamble() {
        let dir = temp_dir("preamble");
        fs::write(dir.join("AGENTS.md"), "Use `semble search` to find code.").unwrap();

        let prompt = build_system_prompt(&dir, &["read", "bash"]).unwrap();

        assert!(prompt.starts_with("Your only tools are: read, bash"));
        assert!(prompt.contains("semble search"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_sources_means_no_system_prompt() {
        let dir = temp_dir("empty");
        assert!(build_system_prompt(&dir, &["read"]).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
