//! Consensus, debate, and multi-provider orchestration.
//!
//! Providers never get direct tool access — they request evidence through the
//! main agent via `request_evidence`. The main agent executes, caches, and
//! shares results.

use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use futures::future::join_all;

use crate::agent::spinner::SpinnerGuard;
use crate::agent::{AgentContext, messages};
use crate::directives::ToolMode;
use crate::providers::{ProviderRegistry, ProviderRequest};
use crate::tools::{ToolRegistry, ToolResult};

/// Shared context for orchestration operations — avoids passing the same
/// handful of parameters through every function signature.
pub struct OrchestrationCtx<'a> {
    pub provider_registry: &'a Arc<ProviderRegistry>,
    pub tool_registry: &'a ToolRegistry,
    pub system_prompt: &'a Option<String>,
    pub cwd: &'a Path,
    pub session: &'a mut crate::agent::session::Session,
}

// ── Formatting helpers ────────────────────────────────────────────────

fn format_labeled_responses(responses: &[(String, String)]) -> String {
    responses
        .iter()
        .map(|(provider, content)| format!("── {provider} ──\n{}", content.trim()))
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
            let allowed = ["read", "web_search", "shell"];
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

pub async fn run_consensus(
    ctx: &mut OrchestrationCtx<'_>,
    prompt: &str,
    providers: &[String],
    judge: &Option<String>,
    tools: &ToolMode,
) -> Result<()> {
    providers
        .iter()
        .try_for_each(|p| ctx.provider_registry.validate_provider(p))?;
    if let Some(judge) = judge {
        ctx.provider_registry.validate_provider(judge)?;
    }
    validate_orchestration_tools(tools)?;
    ctx.session.push_user(prompt.to_string())?;

    let responses = collect_provider_responses(
        &ProviderCtx {
            provider_registry: ctx.provider_registry,
            tool_registry: ctx.tool_registry,
            system_prompt: ctx.system_prompt,
            cwd: ctx.cwd,
        },
        providers,
        prompt,
        "consensus",
        tools,
    )
    .await?;
    let initial_output = format_labeled_responses(&responses);
    println!("{initial_output}");
    ctx.session.push_assistant(initial_output)?;

    let judge_name = judge
        .as_deref()
        .unwrap_or_else(|| providers[0].as_str())
        .to_string();
    let synthesis = synthesize_consensus(
        ctx.provider_registry,
        ctx.system_prompt,
        &judge_name,
        prompt,
        &responses,
        "Consensus",
    )
    .await?;
    let output = format!("── Consensus ({judge_name}) ──\n{synthesis}");
    println!("\n{output}");
    ctx.session.push_assistant(output)?;
    Ok(())
}

pub async fn run_debate(
    ctx: &mut OrchestrationCtx<'_>,
    prompt: &str,
    providers: &[String],
    judge: &Option<String>,
    rounds: usize,
    tools: &ToolMode,
) -> Result<()> {
    providers
        .iter()
        .try_for_each(|p| ctx.provider_registry.validate_provider(p))?;
    if let Some(judge) = judge {
        ctx.provider_registry.validate_provider(judge)?;
    }
    validate_orchestration_tools(tools)?;
    ctx.session.push_user(prompt.to_string())?;

    let pctx = ProviderCtx {
        provider_registry: ctx.provider_registry,
        tool_registry: ctx.tool_registry,
        system_prompt: ctx.system_prompt,
        cwd: ctx.cwd,
    };

    let mut transcript =
        collect_provider_responses(&pctx, providers, prompt, "initial answer", tools).await?;
    let mut output = format!(
        "── Round 1: Initial Answers ──\n\n{}",
        format_labeled_responses(&transcript)
    );
    println!("{output}");

    for round in 1..=rounds {
        let debate_prompt = format_debate_round_prompt(prompt, &transcript, round);
        let critiques = collect_provider_responses(
            &pctx,
            providers,
            &debate_prompt,
            "critique/revision",
            tools,
        )
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

    let judge_name = judge
        .as_deref()
        .unwrap_or_else(|| providers[0].as_str())
        .to_string();
    let synthesis = synthesize_consensus(
        ctx.provider_registry,
        ctx.system_prompt,
        &judge_name,
        prompt,
        &transcript,
        "Final Consensus",
    )
    .await?;
    let output = format!("── Final Consensus ({judge_name}) ──\n{synthesis}");
    println!("\n{output}");
    ctx.session.push_assistant(output)?;
    Ok(())
}

async fn synthesize_consensus(
    provider_registry: &ProviderRegistry,
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
    };

    let spinner = SpinnerGuard::new("synthesizing consensus...");
    let response = provider_registry.complete_once(judge, request).await;
    spinner.stop();
    response.map(|r| r.content)
}

/// Immutable context for provider response collection (no session needed).
struct ProviderCtx<'a> {
    provider_registry: &'a Arc<ProviderRegistry>,
    tool_registry: &'a ToolRegistry,
    system_prompt: &'a Option<String>,
    cwd: &'a Path,
}

async fn collect_provider_responses(
    pctx: &ProviderCtx<'_>,
    providers: &[String],
    prompt: &str,
    purpose: &str,
    tools: &ToolMode,
) -> Result<Vec<(String, String)>> {
    // If tools:none, run a simple single-call per provider (no evidence loop).
    if matches!(tools, ToolMode::None) {
        return collect_provider_responses_no_tools(
            pctx.provider_registry,
            pctx.system_prompt,
            providers,
            prompt,
            purpose,
        )
        .await;
    }

    let max_iterations: usize = env::var("ONELOOP_MAX_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    let allowed = crate::agent::evidence::allowed_tools(tools);
    let cache = crate::agent::evidence::shared_cache();
    let evidence_tool_def = crate::agent::evidence::tool_definition();

    let spinner = SpinnerGuard::new(&format!("multi-model {purpose}..."));

    // Run providers in parallel, each with its own evidence loop.
    let handles: Vec<_> = providers
        .iter()
        .map(|provider_name| {
            let provider_name = provider_name.clone();
            let provider_registry = pctx.provider_registry.clone();
            let tool_registry = pctx.tool_registry.clone();
            let system_prompt = pctx.system_prompt.clone();
            let cwd = pctx.cwd.to_path_buf();
            let cache = cache.clone();
            let allowed = allowed.clone();
            let evidence_tool_def = evidence_tool_def.clone();
            let prompt_text = prompt.to_string();

            tokio::spawn(async move {
                let mut req = ProviderRequest {
                    system_prompt,
                    messages: vec![messages::Message::User(messages::UserMessage {
                        content: prompt_text,
                    })],
                    tools: vec![evidence_tool_def],
                };
                let provider_label = provider_name.clone();
                let ctx = AgentContext { cwd };

                for iteration in 0..max_iterations {
                    let response = provider_registry
                        .complete_once(&provider_name, req.clone())
                        .await?;

                    // No tool calls → final answer.
                    if response.tool_calls.is_empty() {
                        return Ok::<_, anyhow::Error>((provider_label, response.content));
                    }

                    // Process each evidence request through the cache.
                    let mut tool_results: Vec<ToolResult> = Vec::new();
                    for tc in &response.tool_calls {
                        if tc.name != "request_evidence" {
                            tool_results.push(ToolResult {
                                content: "Unknown tool. Use request_evidence to request information.".to_string(),
                                is_error: true,
                            });
                            continue;
                        }

                        let evidence_tool = tc.arguments.get("tool")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let evidence_args = tc.arguments.get("args")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        let description = tc.arguments.get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        let label = crate::agent::evidence::format_request(description, evidence_tool, &evidence_args);
                        let cached = cache
                            .lock()
                            .map(|cache| cache.has(evidence_tool, &evidence_args))
                            .unwrap_or(false);
                        let cache_tag = if cached { " (cached)" } else { "" };

                        let result = crate::agent::evidence::execute(
                            evidence_tool,
                            &evidence_args,
                            &allowed,
                            &cache,
                            &tool_registry,
                            &ctx,
                        )
                        .await;

                        if result.is_error {
                            eprintln!("\x1b[90m    {provider_label} ✗ {label}{cache_tag}\x1b[0m");
                        } else {
                            let lines = result.content.lines().count();
                            let bytes = result.content.len();
                            eprintln!("\x1b[90m    {provider_label} ✓ {label} ({lines} lines, {bytes} bytes){cache_tag}\x1b[0m");
                        }

                        tool_results.push(result);
                    }

                    // Append tool call + result messages.
                    // Only include assistant text if non-empty — Anthropic rejects
                    // empty text blocks ("text content blocks must be non-empty").
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

                    if iteration == max_iterations - 1 {
                        return Ok((provider_label, response.content));
                    }
                }

                Ok((provider_label, String::new()))
            })
        })
        .collect();

    let results = join_all(handles).await;
    spinner.stop();
    results
        .into_iter()
        .map(|res| match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(join_err) => bail!("provider task failed: {join_err}"),
        })
        .collect()
}

/// Simple single-call per provider, no tools.
async fn collect_provider_responses_no_tools(
    provider_registry: &Arc<ProviderRegistry>,
    system_prompt: &Option<String>,
    providers: &[String],
    prompt: &str,
    purpose: &str,
) -> Result<Vec<(String, String)>> {
    let request = ProviderRequest {
        system_prompt: system_prompt.clone(),
        messages: vec![messages::Message::User(messages::UserMessage {
            content: prompt.to_string(),
        })],
        tools: Vec::new(),
    };

    let spinner = SpinnerGuard::new(&format!("multi-model {purpose}..."));
    let provider_registry = provider_registry.clone();
    let handles: Vec<_> = providers
        .iter()
        .map(|provider_name| {
            let provider_name = provider_name.clone();
            let provider_registry = provider_registry.clone();
            let request = request.clone();
            async move {
                let response = provider_registry
                    .complete_once(&provider_name, request)
                    .await?;
                Ok::<_, anyhow::Error>((provider_name, response.content))
            }
        })
        .collect();

    let results = join_all(handles).await;
    spinner.stop();
    results.into_iter().collect()
}
