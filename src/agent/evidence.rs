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

    #[cfg(test)]
    pub fn get(&self, tool: &str, args: &Value) -> Option<&CachedEvidence> {
        self.entries.get(&Self::key(tool, args))
    }

    pub fn has(&self, tool: &str, args: &Value) -> bool {
        self.entries.contains_key(&Self::key(tool, args))
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

// ── Allowed tools from ToolMode ───────────────────────────────────────

/// Resolve which evidence tools are permitted based on the directive's ToolMode.
pub fn allowed_tools(mode: &ToolMode) -> HashSet<&'static str> {
    match mode {
        ToolMode::Default => HashSet::from(["read", "web_search", "shell"]),
        ToolMode::None => HashSet::new(),
        ToolMode::AllowList(names) => names
            .iter()
            .filter_map(|n| match n.as_str() {
                "read" => Some("read"),
                "web_search" => Some("web_search"),
                "shell" => Some("shell"),
                _ => None,
            })
            .collect(),
    }
}

// ── Tool definition ───────────────────────────────────────────────────

/// The single tool definition presented to providers during orchestration.
pub fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "request_evidence".to_string(),
        description: "Request information from the agent to gather evidence for your answer. \
                      Describe what you need and specify the tool and arguments. \
                      The agent will execute the request and return results. \
                      Available tools: 'read' (file contents, args: {path}), \
                      'web_search' (web search, args: {query}), \
                      'shell' (read-only shell command like find/grep/git log, args: {command})."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "What information you need and why"
                },
                "tool": {
                    "type": "string",
                    "enum": ["read", "web_search", "shell"],
                    "description": "Which tool to use"
                },
                "args": {
                    "type": "object",
                    "description": "Tool arguments",
                    "properties": {
                        "path": { "type": "string", "description": "File path (for read)" },
                        "query": { "type": "string", "description": "Search query (for web_search)" },
                        "command": { "type": "string", "description": "Shell command (for shell)" }
                    }
                }
            },
            "required": ["description", "tool", "args"]
        }),
    }
}

// ── Evidence execution ────────────────────────────────────────────────

/// Execute an evidence request: check cache, validate, execute, cache result.
pub async fn execute(
    evidence_tool: &str,
    args: &Value,
    allowed: &HashSet<&'static str>,
    cache: &SharedCache,
    tool_registry: &ToolRegistry,
    ctx: &AgentContext,
) -> ToolResult {
    // Check if this tool is allowed.
    if !allowed.contains(evidence_tool) {
        return ToolResult {
            content: format!(
                "blocked: '{}' is not available in this orchestration. \
                 Allowed tools: {}. Adjust your request.",
                evidence_tool,
                allowed.iter().copied().collect::<Vec<_>>().join(", ")
            ),
            is_error: true,
        };
    }

    // Check cache.
    {
        let Ok(cache_guard) = cache.lock() else {
            return ToolResult {
                content: "evidence cache unavailable: lock poisoned".to_string(),
                is_error: true,
            };
        };
        if cache_guard.has(evidence_tool, args)
            && let Some(cached) = cache_guard
                .entries
                .get(&EvidenceCache::key(evidence_tool, args))
        {
            return ToolResult {
                content: format!("{}\n(cached)", cached.content),
                is_error: cached.is_error,
            };
        }
    }

    // Execute.
    let result = execute_inner(evidence_tool, args, tool_registry, ctx).await;

    // Cache result.
    {
        let Ok(mut cache) = cache.lock() else {
            return ToolResult {
                content: format!("{}\n(cache unavailable: lock poisoned)", result.content),
                is_error: true,
            };
        };
        cache.insert(evidence_tool, args, result.content.clone(), result.is_error);
    }

    result
}

async fn execute_inner(
    tool: &str,
    args: &Value,
    tool_registry: &ToolRegistry,
    ctx: &AgentContext,
) -> ToolResult {
    match tool {
        "read" => {
            let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                return ToolResult {
                    content: "read requires a 'path' argument".to_string(),
                    is_error: true,
                };
            };
            match tool_registry
                .execute("read", json!({"path": path}), ctx)
                .await
            {
                Ok(r) => r,
                Err(e) => ToolResult {
                    content: format!("read failed: {e:#}"),
                    is_error: true,
                },
            }
        }
        "web_search" => {
            let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                return ToolResult {
                    content: "web_search requires a 'query' argument".to_string(),
                    is_error: true,
                };
            };
            match tool_registry
                .execute("web_search", json!({"query": query}), ctx)
                .await
            {
                Ok(r) => r,
                Err(e) => ToolResult {
                    content: format!("web_search failed: {e:#}"),
                    is_error: true,
                },
            }
        }
        "shell" => {
            let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
                return ToolResult {
                    content: "shell requires a 'command' argument".to_string(),
                    is_error: true,
                };
            };
            if !is_safe_shell_command(command) {
                return ToolResult {
                    content: format!(
                        "blocked: command '{command}' is not allowed. \
                         Only read-only inspection commands are permitted \
                         (find, grep, rg, git log, ls, cat, wc, etc.). \
                         Rephrase your request or ask for specific information."
                    ),
                    is_error: true,
                };
            }
            match tool_registry
                .execute("bash", json!({"command": command}), ctx)
                .await
            {
                Ok(r) => r,
                Err(e) => ToolResult {
                    content: format!("shell failed: {e:#}"),
                    is_error: true,
                },
            }
        }
        other => ToolResult {
            content: format!("unknown evidence tool: {other}"),
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
        "xargs",
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
        "tee",
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

fn base_command(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

pub fn is_safe_shell_command(command: &str) -> bool {
    let base = base_command(command);
    if base == "git" {
        let parts: Vec<&str> = command.split_whitespace().collect();
        let sub = parts.iter().skip(1).find(|p| !p.starts_with('-'));
        return sub.is_some_and(|s| GIT_READ_ONLY.contains(s));
    }
    SAFE_COMMANDS.contains(base)
}

// ── Format helper ─────────────────────────────────────────────────────

/// Format an evidence request for display to the user.
pub fn format_request(description: &str, tool: &str, args: &Value) -> String {
    match tool {
        "read" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("read: {path}")
        }
        "web_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            format!("search: {query}")
        }
        "shell" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            format!("shell: {cmd}")
        }
        _ => description.to_string(),
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
        assert!(set.contains("web_search"));
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
            "web_search".to_string(),
        ]));
        assert!(set.contains("read"));
        assert!(set.contains("web_search"));
        assert!(!set.contains("shell"));
    }

    #[test]
    fn format_request_display() {
        assert_eq!(
            format_request("", "read", &json!({"path": "src/main.rs"})),
            "read: src/main.rs"
        );
        assert_eq!(
            format_request("", "web_search", &json!({"query": "rust async"})),
            "search: rust async"
        );
        assert_eq!(
            format_request("", "shell", &json!({"command": "find . -name '*.rs'"})),
            "shell: find . -name '*.rs'"
        );
    }
}
