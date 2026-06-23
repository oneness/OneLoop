use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    pub system_prompt: Option<String>,
    pub prompt_sources: Vec<&'static str>,
}

impl Default for Config {
    fn default() -> Self {
        let cwd = env::var("ONELOOP_ORIGINAL_DIR")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())
            .unwrap_or_else(|_| PathBuf::from("."));
        let system_prompt = build_system_prompt(&cwd);
        let prompt_sources = prompt_sources(&cwd);
        Self {
            cwd,
            system_prompt,
            prompt_sources,
        }
    }
}

pub fn build_system_prompt(cwd: &Path) -> Option<String> {
    let agents = load_file(cwd.join("AGENTS.md").as_path());
    let memory = load_file(cwd.join(".oneloop").join("memory.md").as_path())
        .map(|m| format!("## Memory\n\n{m}"));

    match (agents, memory) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(m)) => Some(m),
        (Some(a), Some(m)) => Some(format!("{a}\n\n{m}")),
    }
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
