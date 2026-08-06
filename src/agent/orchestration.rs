//! Consensus, debate, and multi-model orchestration.
//!
//! Orchestrated models never get direct tool access — they request evidence
//! through the main agent via `request_evidence`. The main agent executes,
//! caches, and shares results.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use futures::future::join_all;

use crate::agent::evidence::SharedCache;
use crate::agent::spinner::SpinnerGuard;
use crate::agent::{AgentContext, messages};
use crate::directives::ToolMode;
use crate::models::ModelRegistry;
use crate::output::{DIM, RESET};
use crate::providers::ProviderRequest;
use crate::tools::{ToolDefinition, ToolRegistry, ToolResult};

/// Shared context for orchestration operations — avoids passing the same
/// handful of parameters through every function signature.
pub struct OrchestrationCtx<'a> {
    pub models: &'a Arc<ModelRegistry>,
    pub tool_registry: &'a ToolRegistry,
    pub system_prompt: &'a Option<String>,
    pub cwd: &'a Path,
    pub session: &'a mut crate::agent::session::Session,
}

impl<'a> OrchestrationCtx<'a> {
    /// The immutable subset used for collecting model responses.
    fn model_ctx(&self) -> ModelCtx<'a> {
        ModelCtx {
            models: self.models,
            tool_registry: self.tool_registry,
            system_prompt: self.system_prompt,
            cwd: self.cwd,
        }
    }
}

// ── Formatting helpers ────────────────────────────────────────────────

fn format_labeled_responses(responses: &[(String, String)]) -> String {
    responses
        .iter()
        .map(|(model, content)| format!("── {model} ──\n{}", content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_synthesis_prompt(prompt: &str, responses: &[(String, String)], label: &str) -> String {
    format!(
        "The user asked:\n\n{prompt}\n\nSeveral models answered independently:\n\n{}\n\n\
         Synthesize a final {label}. Identify agreements, disagreements, tradeoffs, \
         and a practical recommendation. Do not simply average the answers; prefer the \
         best-supported reasoning.",
        format_labeled_responses(responses)
    )
}

fn format_debate_round_prompt(
    prompt: &str,
    transcript: &[(String, String)],
    round: usize,
) -> String {
    format!(
        "The user asked:\n\n{prompt}\n\nDebate transcript so far:\n\n{}\n\n\
         This is critique/revision round {round}. Critique the other responses, \
         identify where your previous reasoning may be incomplete, and provide a \
         revised position.",
        format_labeled_responses(transcript)
    )
}

// ── Validation ────────────────────────────────────────────────────────

pub fn validate_orchestration_tools(tools: &ToolMode) -> Result<()> {
    match tools {
        ToolMode::Default | ToolMode::None => Ok(()),
        ToolMode::AllowList(names) => {
            let allowed = crate::agent::evidence::allowed_tools(&ToolMode::Default);
            let unsupported: Vec<&str> = names
                .iter()
                .map(String::as_str)
                .filter(|name| !allowed.contains(name))
                .collect();
            if !unsupported.is_empty() {
                bail!(
                    "only read-only evidence tools allowed in orchestration: {}",
                    unsupported.join(", ")
                );
            }
            Ok(())
        }
    }
}

// ── Orchestration ─────────────────────────────────────────────────────

/// Shared opening for consensus and debate: validate the models, judge, and
/// tools, then record the user prompt in the session.
fn begin_orchestration(
    ctx: &mut OrchestrationCtx<'_>,
    prompt: &str,
    models: &[String],
    judge: &Option<String>,
    tools: &ToolMode,
) -> Result<()> {
    for alias in models {
        ctx.models.get(alias)?;
    }
    if let Some(judge) = judge {
        ctx.models.get(judge)?;
    }
    validate_orchestration_tools(tools)?;
    ctx.session.push_user(prompt.to_string())
}

/// Shared closing for consensus and debate: pick the judge (explicit, or the
/// first model), synthesize the transcript, print and record the result.
async fn synthesize_and_record(
    ctx: &mut OrchestrationCtx<'_>,
    judge: &Option<String>,
    models: &[String],
    prompt: &str,
    transcript: &[(String, String)],
    label: &str,
) -> Result<()> {
    let judge_name = judge.as_deref().unwrap_or_else(|| models[0].as_str());
    let synthesis = synthesize_consensus(
        ctx.models,
        ctx.system_prompt,
        judge_name,
        prompt,
        transcript,
        label,
    )
    .await?;
    let output = format!("── {label} ({judge_name}) ──\n{synthesis}");
    println!("\n{output}");
    ctx.session.push_assistant(output)
}

pub async fn run_consensus(
    ctx: &mut OrchestrationCtx<'_>,
    prompt: &str,
    models: &[String],
    judge: &Option<String>,
    tools: &ToolMode,
) -> Result<()> {
    begin_orchestration(ctx, prompt, models, judge, tools)?;

    let responses =
        collect_model_responses(&ctx.model_ctx(), models, prompt, "consensus", tools).await?;
    let initial_output = format_labeled_responses(&responses);
    println!("{initial_output}");
    ctx.session.push_assistant(initial_output)?;

    synthesize_and_record(ctx, judge, models, prompt, &responses, "Consensus").await
}

pub async fn run_debate(
    ctx: &mut OrchestrationCtx<'_>,
    prompt: &str,
    models: &[String],
    judge: &Option<String>,
    rounds: usize,
    tools: &ToolMode,
) -> Result<()> {
    begin_orchestration(ctx, prompt, models, judge, tools)?;

    let mctx = ctx.model_ctx();
    let mut transcript =
        collect_model_responses(&mctx, models, prompt, "initial answer", tools).await?;
    let mut output = format!(
        "── Round 1: Initial Answers ──\n\n{}",
        format_labeled_responses(&transcript)
    );
    println!("{output}");

    for round in 1..=rounds {
        let debate_prompt = format_debate_round_prompt(prompt, &transcript, round);
        let critiques =
            collect_model_responses(&mctx, models, &debate_prompt, "critique/revision", tools)
                .await?;
        let section = format!(
            "── Round {}: Critiques/Revisions ──\n\n{}",
            round + 1,
            format_labeled_responses(&critiques)
        );
        println!("\n{section}");
        output.push_str("\n\n");
        output.push_str(&section);
        transcript.extend(critiques);
    }

    ctx.session.push_assistant(output)?;

    synthesize_and_record(ctx, judge, models, prompt, &transcript, "Final Consensus").await
}

async fn synthesize_consensus(
    models: &ModelRegistry,
    system_prompt: &Option<String>,
    judge: &str,
    prompt: &str,
    responses: &[(String, String)],
    label: &str,
) -> Result<String> {
    let content = format_synthesis_prompt(prompt, responses, label);
    let request = ProviderRequest {
        system_prompt: system_prompt.clone(),
        messages: vec![messages::Message::User(messages::UserMessage { content })],
        tools: Vec::new(),
        model_id_override: None,
    };

    let spinner = SpinnerGuard::new("synthesizing consensus...");
    let response = models.complete_once(judge, request).await;
    spinner.stop();
    response.map(|r| r.content)
}

/// Immutable context for collecting model responses (no session needed).
struct ModelCtx<'a> {
    models: &'a Arc<ModelRegistry>,
    tool_registry: &'a ToolRegistry,
    system_prompt: &'a Option<String>,
    cwd: &'a Path,
}

async fn collect_model_responses(
    mctx: &ModelCtx<'_>,
    models: &[String],
    prompt: &str,
    purpose: &str,
    tools: &ToolMode,
) -> Result<Vec<(String, String)>> {
    // If tools:none, run a simple single call per model (no evidence loop).
    if matches!(tools, ToolMode::None) {
        return collect_model_responses_no_tools(
            mctx.models,
            mctx.system_prompt,
            models,
            prompt,
            purpose,
        )
        .await;
    }

    let max_iterations: usize = crate::config::env_or(
        "ONELOOP_MAX_ITERATIONS",
        crate::config::DEFAULT_MAX_ITERATIONS,
    );

    let allowed = crate::agent::evidence::allowed_tools(tools);
    let cache = crate::agent::evidence::shared_cache();
    let evidence_tool_def = crate::agent::evidence::tool_definition();

    let spinner = SpinnerGuard::new(&format!("multi-model {purpose}..."));

    // Run the models in parallel, each with its own evidence loop.
    let handles: Vec<_> = models
        .iter()
        .map(|alias| {
            tokio::spawn(run_evidence_loop(EvidenceLoop {
                alias: alias.clone(),
                models: mctx.models.clone(),
                tool_registry: mctx.tool_registry.clone(),
                system_prompt: mctx.system_prompt.clone(),
                cwd: mctx.cwd.to_path_buf(),
                cache: cache.clone(),
                allowed: allowed.clone(),
                evidence_tool_def: evidence_tool_def.clone(),
                prompt: prompt.to_string(),
                max_iterations,
            }))
        })
        .collect();

    let results = join_all(handles).await;
    spinner.stop();
    results
        .into_iter()
        .map(|res| match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(join_err) => bail!("model task failed: {join_err}"),
        })
        .collect()
}

/// Owned inputs for one model's evidence loop; each loop runs as its own
/// tokio task.
struct EvidenceLoop {
    alias: String,
    models: Arc<ModelRegistry>,
    tool_registry: ToolRegistry,
    system_prompt: Option<String>,
    cwd: PathBuf,
    cache: SharedCache,
    allowed: HashSet<&'static str>,
    evidence_tool_def: ToolDefinition,
    prompt: String,
    max_iterations: usize,
}

/// Ask one model repeatedly, serving its request_evidence calls through the
/// shared cache, until it gives a final answer or iterations run out.
async fn run_evidence_loop(task: EvidenceLoop) -> Result<(String, String)> {
    let EvidenceLoop {
        alias,
        models,
        tool_registry,
        system_prompt,
        cwd,
        cache,
        allowed,
        evidence_tool_def,
        prompt,
        max_iterations,
    } = task;

    let mut req = ProviderRequest {
        system_prompt,
        messages: vec![messages::Message::User(messages::UserMessage {
            content: prompt,
        })],
        tools: vec![evidence_tool_def],
        model_id_override: None,
    };
    let label = alias.clone();
    let ctx = AgentContext { cwd };

    // If iterations run out while the model is still requesting evidence,
    // its last text (possibly empty) is the best answer available.
    let mut last_content = String::new();
    for _ in 0..max_iterations {
        let response = models.complete_once(&alias, req.clone()).await?;

        // No tool calls → final answer.
        if response.tool_calls.is_empty() {
            return Ok((label, response.content));
        }

        // Process each evidence request through the cache.
        let mut tool_results: Vec<ToolResult> = Vec::new();
        for tc in &response.tool_calls {
            if tc.name != "request_evidence" {
                tool_results.push(ToolResult {
                    content: "Unknown tool. Use request_evidence to request information."
                        .to_string(),
                    is_error: true,
                });
                continue;
            }

            let evidence_tool = tc
                .arguments
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let evidence_args = tc
                .arguments
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let description = tc
                .arguments
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let request_label =
                crate::agent::evidence::format_request(description, evidence_tool, &evidence_args);

            let (result, cached) = crate::agent::evidence::execute(
                evidence_tool,
                &evidence_args,
                &allowed,
                &cache,
                &tool_registry,
                &ctx,
            )
            .await;
            let cache_tag = if cached { " (cached)" } else { "" };

            if result.is_error {
                eprintln!("{DIM}    {label} ✗ {request_label}{cache_tag}{RESET}");
            } else {
                let lines = result.content.lines().count();
                let bytes = result.content.len();
                eprintln!(
                    "{DIM}    {label} ✓ {request_label} ({lines} lines, {bytes} bytes){cache_tag}{RESET}"
                );
            }

            tool_results.push(result);
        }

        // Assistant text only when non-empty: providers have rejected empty
        // text blocks outright.
        use crate::agent::messages::{
            AssistantMessage, Message, ToolCall as MsgToolCall, ToolResultMessage,
        };
        if !response.content.trim().is_empty() {
            req.messages.push(Message::Assistant(AssistantMessage {
                content: response.content.clone(),
            }));
        }
        for tc in &response.tool_calls {
            req.messages.push(Message::ToolCall(MsgToolCall {
                id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
                parse_error: None,
            }));
        }
        for (tc, result) in response.tool_calls.iter().zip(tool_results) {
            req.messages.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                content: result.content,
                is_error: result.is_error,
            }));
        }

        last_content = response.content;
    }

    Ok((label, last_content))
}

/// Simple single call per model, no tools.
async fn collect_model_responses_no_tools(
    models: &Arc<ModelRegistry>,
    system_prompt: &Option<String>,
    aliases: &[String],
    prompt: &str,
    purpose: &str,
) -> Result<Vec<(String, String)>> {
    let request = ProviderRequest {
        system_prompt: system_prompt.clone(),
        messages: vec![messages::Message::User(messages::UserMessage {
            content: prompt.to_string(),
        })],
        tools: Vec::new(),
        model_id_override: None,
    };

    let spinner = SpinnerGuard::new(&format!("multi-model {purpose}..."));
    let handles: Vec<_> = aliases
        .iter()
        .map(|alias| {
            let alias = alias.clone();
            let models = models.clone();
            let request = request.clone();
            async move {
                let response = models.complete_once(&alias, request).await?;
                Ok::<_, anyhow::Error>((alias, response.content))
            }
        })
        .collect();

    let results = join_all(handles).await;
    spinner.stop();
    results.into_iter().collect()
}
