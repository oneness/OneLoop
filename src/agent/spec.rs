//! Everything needed to make one agent, and nothing else.
//!
//! An agent is a value. The loop, the session machinery and the `Tool` trait are
//! shared; what a spec decides is *what* is shared — whose conversation this is,
//! which tools it may call, where it works, and which instructions it reads.
//! A different agent is a different preset, not a second code path.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::prompt::{self, Instruction};
use crate::tools::ToolRegistry;

/// Where an agent's conversation lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPolicy {
    /// The project's own conversation, at `.oneloop/sessions/`.
    Project,
    /// This run only: nothing is read on the way in and nothing is written on
    /// the way out. A one-shot uses one so it neither inherits nor disturbs the
    /// conversation on disk.
    Ephemeral,
}

impl SessionPolicy {
    /// The directory holding this conversation's `sessions/`, the `metrics/`
    /// ledger that costs it, its `errors.log` and its lock — reported by
    /// [`Session::root`] once the session exists. `None` for a conversation that
    /// is never written, which is what decides that a run records nothing.
    pub fn root(&self, cwd: &Path) -> Option<PathBuf> {
        match self {
            Self::Project => Some(cwd.join(".oneloop")),
            Self::Ephemeral => None,
        }
    }
}

/// One agent, resolved.
pub struct AgentSpec {
    /// Where the agent works. Tools resolve paths against it, so the agent owns
    /// it rather than reading it back out of the process.
    pub cwd: PathBuf,
    /// The project instructions this run loaded, in the order they are read.
    /// Empty for a one-shot, which loads none.
    pub instructions: Vec<Instruction>,
    /// The model this agent works as, made active in the registry when the
    /// agent is built. `None` leaves the model the config made active.
    pub model: Option<String>,
    /// The tools this agent may call.
    pub tools: ToolRegistry,
    /// Where this agent's conversation lives.
    pub session: SessionPolicy,
}

impl AgentSpec {
    /// The agent `ol` runs: the project's own conversation, every builtin tool,
    /// the project's own instructions. Every field is what it was before agents
    /// were values.
    pub fn coding(cwd: &Path) -> Result<Self> {
        Ok(Self {
            cwd: cwd.to_path_buf(),
            instructions: prompt::instructions(cwd),
            model: None,
            tools: ToolRegistry::with_builtin_tools(cwd)?,
            session: SessionPolicy::Project,
        })
    }

    /// The same agent for a single prompt: nothing loaded from disk and nothing
    /// written to it.
    ///
    /// This is a statement about the *run*, not about the agent, so it is
    /// applied to whichever spec the run chose rather than being a preset of its
    /// own. A spec says what the agent is; the run mode says whether the
    /// conversation persists.
    pub fn one_shot(mut self) -> Self {
        self.instructions.clear();
        self.session = SessionPolicy::Ephemeral;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::models::ModelRegistry;
    use std::fs;
    use std::sync::Arc;

    /// A registry with two models and no home directory to read.
    fn registry() -> ModelRegistry {
        ModelRegistry::for_test(
            r#"{"default":"main","providers":{"p":{"base_url":"http://u",
                 "api_key_env":"P_KEY","models":{
                   "main":{"id":"main-model"},
                   "astra":{"id":"astra-model"}}}}}"#,
        )
        .unwrap()
    }

    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-spec-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The spec is the whole of what an agent is, so this is the list: the
    /// project's instructions, the model the config made active, every builtin
    /// tool, and the project's own conversation.
    #[test]
    fn the_default_agent_gets_the_projects_conversation_and_every_tool() {
        let dir = project_dir("coding");

        let spec = AgentSpec::coding(&dir).unwrap();

        assert_eq!(spec.session, SessionPolicy::Project);
        assert_eq!(spec.cwd, dir);
        assert!(spec.model.is_none());
        for tool in ["read", "write", "edit", "bash", "elisp"] {
            assert!(
                spec.tools.names().contains(&tool),
                "{:?}",
                spec.tools.names()
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The project's instructions reach the agent: the rules a checkout states
    /// are the ones the model is told to follow.
    #[test]
    fn the_agent_reads_the_projects_instructions() {
        let dir = project_dir("instructions");
        fs::write(dir.join("AGENTS.md"), "prefer small commits").unwrap();

        let spec = AgentSpec::coding(&dir).unwrap();

        assert_eq!(spec.instructions.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A one-shot is a statement about the run, not about the agent: it drops
    /// what would be read or written, and changes nothing else.
    #[test]
    fn a_one_shot_holds_its_conversation_in_memory() {
        let dir = project_dir("oneshot");

        let spec = AgentSpec::coding(&dir).unwrap().one_shot();

        assert_eq!(spec.session, SessionPolicy::Ephemeral);
        assert!(spec.instructions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A spec's model is the one the agent works as: applied to the registry
    /// once, at construction, so the mode line, the gauge, `/model` and the next
    /// request all read one answer. A preference consulted per turn would be a
    /// second answer, and the two would disagree the moment the user typed
    /// `/model`.
    #[test]
    fn a_specs_model_is_the_one_the_agent_starts_on() {
        let dir = project_dir("model");
        let spec = AgentSpec {
            model: Some("astra".to_string()),
            ..AgentSpec::coding(&dir).unwrap()
        };

        let agent = Agent::from_spec(spec, Arc::new(registry())).unwrap();

        assert_eq!(agent.models().active().alias, "astra");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A spec that names a model the config does not have fails where it is
    /// built, naming the alias, rather than on the first turn.
    #[test]
    fn a_spec_naming_an_unknown_model_fails_at_construction() {
        let dir = project_dir("model-unknown");
        let spec = AgentSpec {
            model: Some("nonesuch".to_string()),
            ..AgentSpec::coding(&dir).unwrap()
        };

        let error = Agent::from_spec(spec, Arc::new(registry()))
            .err()
            .expect("an unknown model must not resolve");

        assert!(format!("{error:#}").contains("nonesuch"), "{error:#}");
        let _ = fs::remove_dir_all(&dir);
    }
}
