//! Everything needed to make one agent, and nothing else.
//!
//! An agent is a value. The loop, the session machinery and the `Tool` trait are
//! shared; what a spec decides is *what* is shared — whose conversation this is,
//! which tools it may call, where it works, and which instructions it reads.
//! A second agent is a second spec, not a second code path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::prompt::{self, Instruction};
use crate::tools::{ToolRegistry, bash::BashTool, read::ReadTool};

/// Where an agent's conversation lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPolicy {
    /// The project's own conversation, at `.oneloop/sessions/`. This is what
    /// `ol` has always used, and it stays byte-identical.
    Project,
    /// A named agent's own conversation, at
    /// `.oneloop/agents/<name>/sessions/`, with its own lock beside it. Two
    /// agents in one project therefore occupy two terminals without either
    /// refusing the other.
    Named(&'static str),
    /// This run only: nothing is read on the way in and nothing is written on
    /// the way out. A one-shot uses one so it neither inherits nor disturbs the
    /// conversation on disk.
    Ephemeral {
        /// Whose run it is, when it is not the project's own. A one-shot of a
        /// named agent keeps its name here, so it does not report the coding
        /// agent's session id to the provider.
        namespace: Option<&'static str>,
    },
}

impl SessionPolicy {
    /// The directory holding this conversation's `sessions/`, the `metrics/`
    /// ledger that costs it, its `errors.log` and its lock — reported by
    /// [`Session::root`] once the session exists. `None` for a conversation that
    /// is never written, which is what decides that a run records nothing.
    pub fn root(&self, cwd: &Path) -> Option<PathBuf> {
        match self {
            Self::Project => Some(cwd.join(".oneloop")),
            Self::Named(name) => Some(cwd.join(".oneloop").join("agents").join(name)),
            Self::Ephemeral { .. } => None,
        }
    }

    /// The namespace this conversation's id carries, if any. The project's own
    /// conversation carries none: its id is what it has always been.
    pub fn namespace(&self) -> Option<&'static str> {
        match self {
            Self::Project | Self::Ephemeral { .. } => None,
            Self::Named(name) => Some(name),
        }
    }

    /// The same conversation, held in memory: what a one-shot run gets, since a
    /// prompt on the command line is not part of the conversation on disk.
    ///
    /// The namespace stays, so a one-shot of a named agent still reports its own
    /// id rather than the coding agent's — the run mode decides whether the
    /// conversation persists, not whose conversation it is.
    pub fn one_shot_of(self) -> Self {
        match self {
            Self::Named(name) => Self::Ephemeral {
                namespace: Some(name),
            },
            _ => Self::Ephemeral { namespace: None },
        }
    }
}

/// One agent, resolved.
pub struct AgentSpec {
    /// Where the agent works. Tools and reviewers resolve paths against it, so
    /// the agent owns it rather than reading it back out of the process.
    pub cwd: PathBuf,
    /// The project instructions this run loaded, in the order they are read.
    /// Empty for a one-shot, which loads none.
    pub instructions: Vec<Instruction>,
    /// What this agent is for, in its own words. `None` for the default agent,
    /// whose prompt must stay byte-identical.
    pub persona: Option<String>,
    /// The model this agent works as, made active in the registry when the
    /// agent is built. `None` leaves the model the config made active, which is
    /// what the default agent has always used.
    pub model: Option<String>,
    /// The tools this agent may call.
    pub tools: ToolRegistry,
    /// Where this agent's conversation lives.
    pub session: SessionPolicy,
}

impl AgentSpec {
    /// The default agent: the project's own conversation, every builtin tool,
    /// the project's own instructions. This is what `ol` resolves to, and every
    /// field is what it was before agents were values.
    pub fn coding(cwd: &Path) -> Result<Self> {
        Ok(Self {
            cwd: cwd.to_path_buf(),
            instructions: prompt::instructions(cwd),
            persona: None,
            model: None,
            tools: ToolRegistry::with_builtin_tools(cwd)?,
            session: SessionPolicy::Project,
        })
    }

    /// The worked example of a second agent: a reviewer with its own words, its
    /// own conversation, and no tool that changes a file.
    ///
    /// `bash` is the one way round that, and it is deliberate: a review that
    /// cannot run the check that settles a question is an opinion. What keeps it
    /// from editing is the persona, which says so.
    ///
    /// It reads the project's own instructions, because a review that ignores
    /// the conventions the project states is a review of a different project.
    pub fn reviewer(cwd: &Path) -> Result<Self> {
        Ok(Self {
            cwd: cwd.to_path_buf(),
            instructions: prompt::instructions(cwd),
            persona: Some(prompt::REVIEWER_PERSONA.to_string()),
            // The model the config made active: a reviewer's job is opinion,
            // and the one answering should be the one the user is paying for.
            model: None,
            tools: ToolRegistry::from_tools(vec![Arc::new(ReadTool), Arc::new(BashTool)]),
            session: SessionPolicy::Named("reviewer"),
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
        self.session = self.session.one_shot_of();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::models::ModelRegistry;
    use std::fs;

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

    /// The second agent is a value, and this is all of it: its own words, a tool
    /// set that cannot change anything, and a conversation of its own.
    #[test]
    fn the_reviewer_reads_and_runs_but_cannot_change_anything() {
        let dir = project_dir("reviewer");

        let spec = AgentSpec::reviewer(&dir).unwrap();

        assert_eq!(spec.tools.names(), vec!["read", "bash"]);
        assert_eq!(spec.session, SessionPolicy::Named("reviewer"));
        assert_eq!(spec.cwd, dir);
        assert!(
            spec.persona
                .as_deref()
                .is_some_and(|persona| persona.contains("review")),
            "the reviewer's persona is missing"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The project's instructions reach a reviewer: a review that ignores the
    /// conventions the project states is a review of a different project.
    #[test]
    fn the_reviewer_reads_the_projects_instructions() {
        let dir = project_dir("reviewer-instructions");
        fs::write(dir.join("AGENTS.md"), "prefer small commits").unwrap();

        let spec = AgentSpec::reviewer(&dir).unwrap();

        assert_eq!(spec.instructions.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A one-shot is a statement about the run, not about the agent: it drops
    /// what would be read or written, and keeps whose conversation it is — so a
    /// one-shot reviewer does not report the coding agent's session id.
    #[test]
    fn a_one_shot_of_a_named_agent_keeps_its_namespace() {
        let dir = project_dir("reviewer-oneshot");

        let spec = AgentSpec::reviewer(&dir).unwrap().one_shot();

        assert_eq!(
            spec.session,
            SessionPolicy::Ephemeral {
                namespace: Some("reviewer")
            }
        );
        assert!(spec.instructions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The reviewer's tool set has one way round its own limit, and the
    /// persona's sentence is what closes it. Nothing else does: bash can write,
    /// so if that sentence goes, the agent widens without a line moving here.
    #[test]
    fn the_reviewers_persona_closes_the_bash_escape_hatch() {
        let dir = project_dir("persona-guard");

        let spec = AgentSpec::reviewer(&dir).unwrap();

        assert!(
            spec.persona
                .as_deref()
                .is_some_and(|persona| persona.contains("shell")),
            "the persona no longer says the shell must not stand in for a write tool"
        );
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

    /// The default agent's one-shot carries no namespace, because its id is what
    /// it has always been.
    #[test]
    fn a_one_shot_of_the_default_agent_has_no_namespace() {
        let dir = project_dir("coding-oneshot");

        let spec = AgentSpec::coding(&dir).unwrap().one_shot();

        assert_eq!(spec.session, SessionPolicy::Ephemeral { namespace: None });
        let _ = fs::remove_dir_all(&dir);
    }
}
