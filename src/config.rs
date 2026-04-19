use std::{env, fs, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    pub system_prompt: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let system_prompt = load_agents_md(&cwd);
        Self { cwd, system_prompt }
    }
}

fn load_agents_md(cwd: &PathBuf) -> Option<String> {
    let agents_path = cwd.join("AGENTS.md");
    fs::read_to_string(agents_path).ok()
}
