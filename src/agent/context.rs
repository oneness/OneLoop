use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}
