mod banner;
mod duplicate_reads;
pub mod metrics;
pub mod session;
pub mod spec;
mod spinner;
mod trace;
mod turn;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::messages;
use crate::output;
use crate::{models::ModelRegistry, prompt, providers::errors_log_path};

use spec::AgentSpec;

/// Why the per-prompt loop returned. `Resolved` and `Failed` have already
/// said their piece — an answer was given, or an error was printed — so the
/// cap is the only exit that would otherwise end a run in silence.
#[derive(Clone, Copy)]
enum Stop {
    /// The model answered without asking for more tools.
    Resolved,
    /// The provider refused; its error is already on the terminal.
    Failed,
    /// The iteration cap ran out with tool calls still coming.
    Cap,
}

/// The one line a run the cap cut short prints. `None` for the exits that
/// spoke for themselves.
fn cap_notice(stop: Stop, max_iterations: usize) -> Option<String> {
    matches!(stop, Stop::Cap).then(|| {
        format!(
            "iteration cap reached ({max_iterations}) — this run stopped mid-task; \
             send another prompt to continue, or raise ONELOOP_MAX_ITERATIONS"
        )
    })
}

pub struct Agent {
    /// What this agent is: its working directory, its tools, its instructions
    /// and whose conversation it keeps.
    spec: AgentSpec,
    models: Arc<ModelRegistry>,
    session: session::Session,
    metrics: metrics::Metrics,
    /// The prompt the model is given, built once from the spec. Its length is
    /// the fixed cost the context gauge has to carry.
    system_prompt: String,
    /// Input tokens the server reported for the last successful turn, when it
    /// reported any. Cleared on `/clear`; until a turn runs the gauge falls
    /// back to `estimate_tokens`.
    last_input_tokens: Option<u64>,
    /// Where refusals are recorded: the conversation's own `errors.log`, beside
    /// its session file. None for a one-shot, which writes nothing: a refusal
    /// there would mean creating `.oneloop/` in whatever directory the prompt
    /// ran in.
    errors_log: Option<PathBuf>,
}

impl Agent {
    /// The one constructor. Every field the agent reads comes from the spec, so
    /// a second agent is a second spec rather than a second branch here.
    pub fn from_spec(spec: AgentSpec, models: Arc<ModelRegistry>) -> Result<Self> {
        // Which model this agent works as is the registry's answer, so there is
        // one place to ask: the mode line, the gauge and `/model` all read it.
        // A spec that names none leaves the config's default active, which is
        // what the default agent has always used.
        if let Some(alias) = &spec.model {
            models.set_active(alias)?;
        }

        let system_prompt = prompt::system_prompt(
            &spec.cwd,
            &spec.tools.names(),
            &spec.instructions,
            spec.persona.as_deref(),
        );
        let mut session = session::Session::open(&spec.cwd, spec.session)?;

        // A previous process may have been killed mid-run, leaving tool calls
        // without results that providers would reject. An ephemeral session has
        // none to repair, so this is the same call for both kinds of run.
        Self::close_interrupted_calls(&mut session, "from a previous run")?;

        // Both files follow the conversation: a run that writes one records
        // what it spent beside it and where it was refused, and a run that
        // writes nothing — a one-shot — does neither.
        let metrics = metrics::Metrics::for_session(&session)?;
        let errors_log = session.root().map(errors_log_path);

        Ok(Self {
            spec,
            models,
            session,
            metrics,
            system_prompt,
            last_input_tokens: None,
            errors_log,
        })
    }

    /// Answers tool calls a process died before answering, and says how many
    /// when it answered any. One implementation, for the two times it is owed:
    /// at startup, and after a run was cancelled mid-tool.
    ///
    /// `when` names which death the count belongs to. Only the caller knows —
    /// a startup repair is answering a process that is gone, a cancelled one is
    /// answering the run the reader is sitting in.
    fn close_interrupted_calls(session: &mut session::Session, when: &str) -> Result<()> {
        let repaired = session.repair_dangling_tool_calls()?;
        if repaired > 0 {
            output::step(&format!(
                "closed {repaired} interrupted tool call(s) {when}"
            ));
        }
        Ok(())
    }

    /// Visible history can be shared with an explicitly requested reviewer.
    pub fn messages(&self) -> &[messages::Message] {
        self.session.messages()
    }

    /// Working directory shared by tools and external reviewers.
    pub fn cwd(&self) -> &Path {
        &self.spec.cwd
    }

    /// Record external reference material without starting a model turn.
    pub fn record_reference(&mut self, reference: String) -> Result<()> {
        self.session.push_user(reference)
    }

    pub fn models(&self) -> &ModelRegistry {
        &self.models
    }

    /// Rebuilds the model registry from disk without disturbing the session.
    /// The replacement happens only after the complete configuration is valid.
    pub fn reload_models(&mut self) -> Result<()> {
        let active = self.models.active().alias.clone();
        let models = ModelRegistry::new()?;

        // Keep the user's session choice when it still exists. Otherwise the
        // freshly loaded config's default is already active.
        if models.get(&active).is_ok() {
            models.set_active(&active)?;
        }

        self.models = Arc::new(models);
        Ok(())
    }

    /// A cancelled run leaves calls without results, which the provider
    /// rejects on the next request.
    pub fn repair_dangling_tool_calls(&mut self) -> Result<()> {
        Self::close_interrupted_calls(&mut self.session, "in this run")
    }

    /// Rotates to a new file; the old one stays on disk.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session = self.session.rotate()?;
        self.metrics = metrics::Metrics::for_session(&self.session)?;
        // The measured count belonged to the conversation just rotated away.
        self.last_input_tokens = None;
        output::step(&format!(
            "cleared context, new session: {}",
            self.session.path().display()
        ));
        Ok(())
    }

    fn system_prompt_chars(&self) -> usize {
        self.system_prompt.len()
    }

    /// What the prompt's gauge shows: how much of the context is still free.
    /// Prefers the prompt-token count the server reported for the last turn,
    /// which is measured rather than approximated; falls back to the byte
    /// estimate until a turn has run. `None` when the active model declares no
    /// window — the number would be fiction without one.
    pub fn remaining_context(&self) -> Option<u8> {
        let window = self.models.active().context_window?;
        Some(match self.last_input_tokens {
            Some(tokens) => metrics::remaining_context_from_tokens(tokens, window),
            None => metrics::remaining_context(
                self.session.messages(),
                self.system_prompt_chars(),
                window,
            ),
        })
    }
}
