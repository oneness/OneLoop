mod banner;
mod batch;
mod compact;
mod duplicate_reads;
pub mod metrics;
pub mod session;
pub mod spec;
mod spinner;
mod trace;
mod turn;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use crate::messages;
use crate::output;
use crate::{models::ModelRegistry, prompt, providers::errors_log_path};

use spec::AgentSpec;

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
    /// What the agent was last waiting on — `a response`, or the trace label of
    /// the calls in flight. Nothing inside the future Ctrl+C drops gets to log,
    /// so this is the only thing that can say afterwards whether the wait was
    /// the model or a tool, and which tool.
    waiting_on: String,
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

        let system_prompt =
            prompt::system_prompt(&spec.cwd, &spec.tools.names(), &spec.instructions);
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
            waiting_on: "nothing".to_string(),
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

    /// Rotates to a new file; the old one stays on disk. Returns the new
    /// session's path, so the caller can say which one it got: `/clear` cleared
    /// and `/compact` summarized, and only the caller knows which word is true.
    pub fn clear_session(&mut self) -> Result<PathBuf> {
        self.session = self.session.rotate()?;
        self.metrics = metrics::Metrics::for_session(&self.session)?;
        // The measured count belonged to the conversation just rotated away.
        self.last_input_tokens = None;
        Ok(self.session.path().to_path_buf())
    }

    /// Writes down a turn the user stopped. Everything that would normally log
    /// — the request, the tool batch — is inside the future Ctrl+C drops, so a
    /// cancelled turn is otherwise invisible in the ledger: the one event worth
    /// reading is the one that never gets written.
    pub fn log_cancelled_turn(&self, started: Instant) {
        self.metrics.log(
            "turn_cancelled",
            json!({
                "duration_ms": started.elapsed().as_millis(),
                "model": self.models.active().alias,
                "waiting_on": &self.waiting_on,
            }),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-agent-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn registry() -> ModelRegistry {
        ModelRegistry::for_test(
            r#"{"default":"main","providers":{"p":{"base_url":"http://u",
                 "api_key_env":"P_KEY","models":{"main":{"id":"main-model"}}}}}"#,
        )
        .unwrap()
    }

    /// The one line in the ledger a test reads: a single event has been logged
    /// by then, so the first file is the whole of it.
    fn ledger(dir: &Path) -> String {
        let entry = fs::read_dir(dir.join(".oneloop").join("metrics"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        fs::read_to_string(entry.path()).unwrap()
    }

    /// A turn the user stops writes nothing else: the request that was in
    /// flight is dropped with the future, so this line is the only record of
    /// how long it ran and what it was waiting for — including which call.
    #[test]
    fn a_cancelled_turn_names_the_call_it_was_waiting_on() {
        let dir = project_dir("cancelled");
        let mut agent =
            Agent::from_spec(AgentSpec::coding(&dir).unwrap(), Arc::new(registry())).unwrap();
        agent.waiting_on = "bash: sed -n '1,40p' src/main.rs".to_string();

        agent.log_cancelled_turn(Instant::now());

        let body = ledger(&dir);
        assert!(body.contains("bash: sed -n '1,40p' src/main.rs"), "{body}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The summarizer's request is an `api_call` like any other, and `purpose`
    /// is the only thing that tells the two kinds apart in the ledger — a
    /// compaction is billed the same way and would otherwise read as a turn.
    #[test]
    fn an_api_call_records_what_the_request_was_for() {
        let dir = project_dir("api-call-purpose");
        let agent =
            Agent::from_spec(AgentSpec::coding(&dir).unwrap(), Arc::new(registry())).unwrap();

        agent.log_api_call("main", Instant::now(), 10, None, true, "summarize");

        let body = ledger(&dir);
        assert!(body.contains(r#""purpose":"summarize""#), "{body}");
        let _ = fs::remove_dir_all(&dir);
    }
}
