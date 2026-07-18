//! Evidence agent for multi-model orchestration.
//!
//! Instead of giving providers direct tool access, they ask the main agent
//! to gather evidence on their behalf. The main agent executes, caches,
//! and shares results across all providers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};

use crate::agent::AgentContext;
use crate::directives::ToolMode;
use crate::tools::{ToolDefinition, ToolRegistry, ToolResult};

// ── Cache ─────────────────────────────────────────────────────────────

/// Cache for evidence gathered during multi-model orchestration.
/// Shared across all providers so evidence is gathered once and reused.
pub struct EvidenceCache {
    entries: HashMap<String, CachedEvidence>,
}

pub(crate) struct CachedEvidence {
    pub content: String,
    pub is_error: bool,
}

impl EvidenceCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn key(tool: &str, args: &Value) -> String {
        let mut sorted = serde_json::Map::new();
        if let Value::Object(map) = args {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), map[k].clone());
            }
        }
        format!("{tool}:{}", serde_json::Value::Object(sorted))
    }

    pub fn get(&self, tool: &str, args: &Value) -> Option<&CachedEvidence> {
        self.entries.get(&Self::key(tool, args))
    }

    pub fn insert(&mut self, tool: &str, args: &Value, content: String, is_error: bool) {
        self.entries
            .insert(Self::key(tool, args), CachedEvidence { content, is_error });
    }
}

pub type SharedCache = Arc<Mutex<EvidenceCache>>;

pub fn shared_cache() -> SharedCache {
    Arc::new(Mutex::new(EvidenceCache::new()))
}

// ── Evidence tool table ───────────────────────────────────────────────

/// One evidence tool as offered to providers: the name they see, the registry
/// tool that serves it, and the single string argument it takes.
///
/// This table is the single source of truth — the allowlist, the
/// request_evidence definition, execution dispatch, and display formatting
/// are all derived from it. Adding or renaming an evidence tool is one entry
/// here (plus a guardrail in `execute_inner` if it needs one).
struct EvidenceTool {
    name: &'static str,
    backing_tool: &'static str,
    arg: &'static str,
    /// Short capability summary for the request_evidence description.
    summary: &'static str,
    /// Description of the argument in the schema.
    arg_description: &'static str,
    /// Verb shown when displaying a request to the user.
    display: &'static str,
}

const EVIDENCE_TOOLS: &[EvidenceTool] = &[
    EvidenceTool {
        name: "read",
        backing_tool: "read",
        arg: "path",
        summary: "file contents",
        arg_description: "File path (for read)",
        display: "read",
    },
    EvidenceTool {
        name: "fetch_page",
        backing_tool: "fetch_page",
        arg: "url",
        summary: "fetch web page",
        arg_description: "Page URL (for fetch_page)",
        display: "fetch",
    },
    EvidenceTool {
        name: "shell",
        backing_tool: "bash",
        arg: "command",
        summary: "read-only shell command like find/grep/git log",
        arg_description: "Shell command (for shell)",
        display: "shell",
    },
];

fn evidence_tool(name: &str) -> Option<&'static EvidenceTool> {
    EVIDENCE_TOOLS.iter().find(|tool| tool.name == name)
}

/// Resolve which evidence tools are permitted based on the directive's ToolMode.
pub fn allowed_tools(mode: &ToolMode) -> HashSet<&'static str> {
    let all = EVIDENCE_TOOLS.iter().map(|tool| tool.name);
    match mode {
        ToolMode::Default => all.collect(),
        ToolMode::None => HashSet::new(),
        ToolMode::AllowList(names) => all
            .filter(|name| names.iter().any(|allowed| allowed == name))
            .collect(),
    }
}

/// The single tool definition presented to providers during orchestration.
pub fn tool_definition() -> ToolDefinition {
    let tool_list = EVIDENCE_TOOLS
        .iter()
        .map(|tool| format!("'{}' ({}, args: {{{}}})", tool.name, tool.summary, tool.arg))
        .collect::<Vec<_>>()
        .join(", ");
    let names: Vec<&str> = EVIDENCE_TOOLS.iter().map(|tool| tool.name).collect();
    let arg_properties: serde_json::Map<String, Value> = EVIDENCE_TOOLS
        .iter()
        .map(|tool| {
            (
                tool.arg.to_string(),
                json!({ "type": "string", "description": tool.arg_description }),
            )
        })
        .collect();

    ToolDefinition {
        name: "request_evidence".to_string(),
        description: format!(
            "Request information from the agent to gather evidence for your answer. \
             Describe what you need and specify the tool and arguments. \
             The agent will execute the request and return results. \
             Available tools: {tool_list}."
        ),
        schema: json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "What information you need and why"
                },
                "tool": {
                    "type": "string",
                    "enum": names,
                    "description": "Which tool to use"
                },
                "args": {
                    "type": "object",
                    "description": "Tool arguments",
                    "properties": arg_properties
                }
            },
            "required": ["description", "tool", "args"]
        }),
    }
}

// ── Evidence execution ────────────────────────────────────────────────

/// Execute an evidence request: check cache, validate, execute, cache result.
/// The bool is true when the result was served from the cache.
pub async fn execute(
    evidence_tool: &str,
    args: &Value,
    allowed: &HashSet<&'static str>,
    cache: &SharedCache,
    tool_registry: &ToolRegistry,
    ctx: &AgentContext,
) -> (ToolResult, bool) {
    // Check if this tool is allowed.
    if !allowed.contains(evidence_tool) {
        return (
            ToolResult {
                content: format!(
                    "blocked: '{}' is not available in this orchestration. \
                     Allowed tools: {}. Adjust your request.",
                    evidence_tool,
                    allowed.iter().copied().collect::<Vec<_>>().join(", ")
                ),
                is_error: true,
            },
            false,
        );
    }

    // Check cache.
    if let Ok(cache_guard) = cache.lock()
        && let Some(cached) = cache_guard.get(evidence_tool, args)
    {
        return (
            ToolResult {
                content: format!("{}\n(cached)", cached.content),
                is_error: cached.is_error,
            },
            true,
        );
    }

    // Execute.
    let result = execute_inner(evidence_tool, args, tool_registry, ctx).await;

    // Cache the result. A poisoned lock only loses caching — the evidence
    // itself is still good, so return it unchanged.
    if let Ok(mut cache) = cache.lock() {
        cache.insert(evidence_tool, args, result.content.clone(), result.is_error);
    }

    (result, false)
}

async fn execute_inner(
    tool: &str,
    args: &Value,
    tool_registry: &ToolRegistry,
    ctx: &AgentContext,
) -> ToolResult {
    let Some(spec) = evidence_tool(tool) else {
        return ToolResult {
            content: format!("unknown evidence tool: {tool}"),
            is_error: true,
        };
    };
    let Some(value) = args.get(spec.arg).and_then(|v| v.as_str()) else {
        return ToolResult {
            content: format!("{} requires a '{}' argument", spec.name, spec.arg),
            is_error: true,
        };
    };
    if spec.name == "shell" && !is_safe_shell_command(value) {
        return ToolResult {
            content: format!(
                "blocked: command '{value}' is not allowed. \
                 Only read-only inspection commands are permitted \
                 (find, grep, rg, git log, ls, cat, wc, etc.). \
                 Rephrase your request or ask for specific information."
            ),
            is_error: true,
        };
    }

    let backing_args = Value::Object(
        std::iter::once((spec.arg.to_string(), Value::String(value.to_string()))).collect(),
    );
    match tool_registry.execute(spec.backing_tool, backing_args, ctx).await {
        Ok(result) => result,
        Err(e) => ToolResult {
            content: format!("{} failed: {e:#}", spec.name),
            is_error: true,
        },
    }
}

// ── Shell safety ──────────────────────────────────────────────────────

static SAFE_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "cat",
        "head",
        "tail",
        "less",
        "wc",
        "file",
        "stat",
        "ls",
        "tree",
        "du",
        "df",
        "find",
        "grep",
        "rg",
        "ag",
        "ack",
        "fd",
        "which",
        "type",
        "git",
        "sort",
        "uniq",
        "cut",
        "tr",
        "awk",
        "sed",
        "jq",
        "yq",
        "echo",
        "printf",
        "date",
        "uname",
        "hostname",
        "whoami",
        "env",
        "printenv",
        "pwd",
        "diff",
        "comm",
        "paste",
        "column",
        "fmt",
        "fold",
        "expand",
        "unexpand",
        "basename",
        "dirname",
        "realpath",
        "readlink",
        "test",
        "[",
        "nl",
        "od",
        "hexdump",
        "xxd",
        "strings",
        "md5sum",
        "sha256sum",
        "shasum",
        "cksum",
    ])
});

static GIT_READ_ONLY: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "log",
        "diff",
        "show",
        "status",
        "branch",
        "tag",
        "remote",
        "stash",
        "blame",
        "shortlog",
        "describe",
        "reflog",
        "ls-files",
        "ls-tree",
        "ls-remote",
        "rev-list",
        "rev-parse",
        "name-rev",
        "merge-base",
        "cherry",
        "grep",
        "cat-file",
        "show-ref",
        "for-each-ref",
        "count-objects",
        "verify-pack",
        "fsck",
        "whatchanged",
    ])
});

/// Sequences that can smuggle writes past the per-stage base-command check:
/// command substitution, chaining, backgrounding, and redirection.
const BLOCKED_SEQUENCES: &[&str] = &["$(", "`", ";", "&", ">", "<", "\n"];

/// Best-effort guardrail, not a security boundary. It stops a cooperating
/// model from running obviously state-changing commands, but some allowed
/// commands retain exec escape hatches (`awk 'system(...)'`, `sed e`,
/// `find -exec`). Treat it as a seatbelt, not a sandbox.
pub fn is_safe_shell_command(command: &str) -> bool {
    if BLOCKED_SEQUENCES.iter().any(|seq| command.contains(seq)) {
        return false;
    }
    // Check every stage of a pipeline, not just the first command.
    command.split('|').all(is_safe_pipeline_stage)
}

fn is_safe_pipeline_stage(stage: &str) -> bool {
    let mut words = stage.split_whitespace();
    let Some(base) = words.next() else {
        return false;
    };
    if base == "git" {
        let sub = words.find(|word| !word.starts_with('-'));
        return sub.is_some_and(|s| GIT_READ_ONLY.contains(s));
    }
    SAFE_COMMANDS.contains(base)
}

// ── Format helper ─────────────────────────────────────────────────────

/// Format an evidence request for display to the user.
pub fn format_request(description: &str, tool: &str, args: &Value) -> String {
    match evidence_tool(tool) {
        Some(spec) => {
            let value = args.get(spec.arg).and_then(|v| v.as_str()).unwrap_or("?");
            format!("{}: {value}", spec.display)
        }
        None => description.to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_commands_pass() {
        assert!(is_safe_shell_command("find . -name '*.rs'"));
        assert!(is_safe_shell_command("grep -rn 'TODO' src/"));
        assert!(is_safe_shell_command("rg 'pattern' src/"));
        assert!(is_safe_shell_command("ls -la"));
        assert!(is_safe_shell_command("cat README.md"));
        assert!(is_safe_shell_command("wc -l src/main.rs"));
        assert!(is_safe_shell_command("git log --oneline -10"));
        assert!(is_safe_shell_command("git diff HEAD~1"));
        assert!(is_safe_shell_command("git status"));
        assert!(is_safe_shell_command("git blame src/main.rs"));
        assert!(is_safe_shell_command("tree src/"));
        assert!(is_safe_shell_command("du -sh ."));
        assert!(is_safe_shell_command("jq '.name' package.json"));
        assert!(is_safe_shell_command("sort file.txt | uniq"));
    }

    #[test]
    fn blocked_commands_fail() {
        assert!(!is_safe_shell_command("rm -rf /"));
        assert!(!is_safe_shell_command("curl http://example.com"));
        assert!(!is_safe_shell_command("python3 -c 'import os'"));
        assert!(!is_safe_shell_command("npm install"));
        assert!(!is_safe_shell_command("git push"));
        assert!(!is_safe_shell_command("git commit -m 'hack'"));
        assert!(!is_safe_shell_command("git reset --hard"));
        assert!(!is_safe_shell_command("git clean -fd"));
        assert!(!is_safe_shell_command("mv file.txt /tmp"));
        assert!(!is_safe_shell_command("chmod 777 file"));
        assert!(!is_safe_shell_command("docker run ubuntu"));
    }

    #[test]
    fn chained_and_substituted_commands_fail() {
        assert!(!is_safe_shell_command("cat x; rm -rf /"));
        assert!(!is_safe_shell_command("cat x && rm -rf /"));
        assert!(!is_safe_shell_command("echo $(rm -rf /)"));
        assert!(!is_safe_shell_command("cat `whoami`"));
        assert!(!is_safe_shell_command("echo pwned > /etc/passwd"));
        assert!(!is_safe_shell_command("grep -r TODO . & rm -rf /"));
    }

    #[test]
    fn every_pipeline_stage_is_checked() {
        assert!(is_safe_shell_command("git log --oneline | head -5"));
        assert!(!is_safe_shell_command("cat file.txt | python3"));
        assert!(!is_safe_shell_command("find . -name '*.sh' | xargs rm"));
    }

    #[test]
    fn cache_deduplicates() {
        let mut cache = EvidenceCache::new();
        let args = json!({"path": "src/main.rs"});

        assert!(cache.get("read", &args).is_none());

        cache.insert("read", &args, "file contents here".to_string(), false);
        let cached = cache.get("read", &args).unwrap();
        assert_eq!(cached.content, "file contents here");
        assert!(!cached.is_error);

        let cached2 = cache.get("read", &args).unwrap();
        assert_eq!(cached2.content, "file contents here");
    }

    #[test]
    fn cache_keys_are_order_independent() {
        let mut cache = EvidenceCache::new();
        let args1 = json!({"path": "a.rs", "offset": 10});
        let args2 = json!({"offset": 10, "path": "a.rs"});

        cache.insert("read", &args1, "content".to_string(), false);

        assert!(cache.get("read", &args2).is_some());
    }

    #[test]
    fn default_allowed_tools() {
        let set = allowed_tools(&ToolMode::Default);
        assert!(set.contains("read"));
        assert!(set.contains("fetch_page"));
        assert!(set.contains("shell"));
        assert!(!set.contains("bash"));
    }

    #[test]
    fn none_allowed_tools() {
        let set = allowed_tools(&ToolMode::None);
        assert!(set.is_empty());
    }

    #[test]
    fn allowlist_tools() {
        let set = allowed_tools(&ToolMode::AllowList(vec![
            "read".to_string(),
            "fetch_page".to_string(),
        ]));
        assert!(set.contains("read"));
        assert!(set.contains("fetch_page"));
        assert!(!set.contains("shell"));
    }

    #[test]
    fn format_request_display() {
        assert_eq!(
            format_request("", "read", &json!({"path": "src/main.rs"})),
            "read: src/main.rs"
        );
        assert_eq!(
            format_request("", "fetch_page", &json!({"url": "https://example.com"})),
            "fetch: https://example.com"
        );
        assert_eq!(
            format_request("", "shell", &json!({"command": "find . -name '*.rs'"})),
            "shell: find . -name '*.rs'"
        );
    }
}
