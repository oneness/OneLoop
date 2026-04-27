# Plugin System Design

Lua-based plugin system for extending oneloop's behavior without modifying core agent code.

## Why Lua

| Factor | Lua | WASM | JavaScript | Python |
|---|---|---|---|---|
| Runtime size | ~200KB | ~15MB | ~30-50MB | ~30MB |
| REPL support | ✅ trivial | ❌ compile ahead | ✅ | ✅ |
| Dependency | `mlua` crate | `wasmtime` | `deno_core`/`boa` | `PyO3` |
| Learning curve | Low | High | Low | Low |
| Sandboxing | Process-level | ✅ native | Process-level | Process-level |
| Proven model | Neovim, Redis, Nginx | Fastly, Shopify | Deno | — |

Lua is the Neovim play: lightweight runtime, proven ecosystem, REPL-friendly, and `mlua` has first-class `serde` integration for passing Rust structs to/from Lua seamlessly.

## Setup

Add `mlua` with `lua54` feature to `Cargo.toml`:

```toml
[dependencies]
mlua = { version = "0.10", features = ["lua54", "vendored", "serde"] }
```

## Plugin Discovery

On startup, load all `.lua` files from:

1. `.oneloop/plugins/` (project-local)
2. `~/.oneloop/plugins/` (user-global)

Each `.lua` file must export a table with a `hooks` field listing which lifecycle hooks it subscribes to:

```lua
local M = {}
M.hooks = { "before_api_call", "after_tool" }
-- define hook functions...
return M
```

## Hot Reload

Plugins are loaded once on startup. Editing a `.lua` file while the agent is running does **not** take effect automatically. Use the `/reload` REPL command to re-read all plugin files from disk:

```
> /reload
  ✓ reloaded 3 plugins
```

This calls `PluginManager::load_from_dirs()` again, replacing all loaded plugins with the current files on disk. The agent loop continues uninterrupted — the next hook fire uses the new code. No restart needed.

This is the simplest approach: explicit, predictable, no surprising behavior mid-conversation.

## Host Side (Rust)

```rust
// src/plugin.rs

struct PluginManager {
    lua: mlua::Lua,
    plugins: Vec<PluginInfo>,
}

impl PluginManager {
    /// Load all .lua files from .oneloop/plugins/ and ~/.oneloop/plugins/
    fn load_from_dirs(project_dir: &Path, home_dir: &Path) -> Result<Self> { ... }

    /// Fire a hook. Collect all non-nil return values.
    fn fire_hook(&self, hook: &str, args: HookArgs) -> Vec<String> {
        // For each plugin subscribed to this hook:
        //   call plugin[hook](args)
        //   collect non-nil returns as strings
    }
}
```

### Hook Integration Points in `agent.rs`

```rust
// on_start — when agent boots
plugins.fire_hook("on_start", &ctx);

// before_api_call — before sending request to provider
let extras = plugins.fire_hook("before_api_call", &messages, &ctx);
for extra in extras { system_prompt.push_str(&extra); }

// after_api_call — after receiving response
let warnings = plugins.fire_hook("after_api_call", provider, model, tokens, &ctx);
for w in warnings { println!("{w}"); }

// before_tool — before executing a tool (can block/confirm)
let action = plugins.fire_hook("before_tool", tool_name, args, &ctx);
match action {
    Some(Action::Block(msg)) => return ToolResult { content: msg, is_error: true },
    Some(Action::Confirm(msg)) => { /* prompt user */ },
    None => { /* proceed */ },
}

// after_tool — after tool completes (can inject messages)
let inject = plugins.fire_hook("after_tool", tool_name, args, result, &ctx);
for msg in inject { session.push_user(msg)?; }

// on_compact — after compaction completes
plugins.fire_hook("on_compact", &summary, &ctx);
```

## Context API (Lua → Rust)

Plugins receive a `ctx` object with these methods:

```lua
ctx:shell(command)         -- Run a shell command, return stdout
ctx:read_file(path)        -- Read a file's contents (nil if missing)
ctx:write_file(path, text) -- Write to a file
ctx:append_file(path, text)-- Append to a file
ctx:file_exists(path)      -- Check if a file exists
ctx:resolve_path(path)     -- Resolve to absolute path
ctx:project_root           -- The project root directory
ctx:log(message)           -- Write to oneloop's log (non-intrusive)
```

## Hook Reference

| Hook | Signature | Return | Used For |
|---|---|---|---|
| `on_start` | `(ctx)` | nil | Initialize state, create files |
| `before_api_call` | `(messages, ctx)` | string or nil | Inject context into system prompt |
| `after_api_call` | `(provider, model, tokens_in, tokens_out, ctx)` | string or nil | Cost tracking, logging |
| `before_tool` | `(tool_name, arguments, ctx)` | nil / `{action="block", message=...}` / `{action="confirm", message=...}` | Safety rails, validation |
| `after_tool` | `(tool_name, arguments, result, ctx)` | string or nil | Auto-test, auto-lint, inject results |
| `on_compact` | `(summary, ctx)` | nil | Persist plugin state, extract facts |

---

## Example: Memory Plugin

Persistent memory that survives compaction and new sessions. Before every API call, injects `memory.md` into the system prompt. After compaction, extracts key facts and appends them to memory.

```lua
-- .oneloop/plugins/memory.lua

local M = {}
M.hooks = { "on_start", "before_api_call", "on_compact" }

local MEMORY_FILE = ".oneloop/memory.md"

local function read_memory(ctx)
    return ctx:read_file(MEMORY_FILE)
end

local function append_memory(ctx, content)
    ctx:append_file(MEMORY_FILE, "\n" .. content .. "\n")
end

function M.on_start(ctx)
    local memory = read_memory(ctx)
    if not memory then
        ctx:write_file(MEMORY_FILE, [[
# Project Memory

## User Preferences
_(collected automatically from conversations)_

## Architecture Notes
_(key files, patterns, decisions)_

## Current Work
_(what's in progress)_

## Key Decisions
_(why things were done a certain way)_
]])
        ctx:log("memory: created .oneloop/memory.md template")
    end
end

function M.before_api_call(messages, ctx)
    local memory = read_memory(ctx)
    if not memory or memory:len() == 0 then
        return nil
    end
    return "Project memory (persists across sessions):\n\n" .. memory
end

function M.on_compact(summary, ctx)
    local facts = {}

    for pref in summary:gmatch("user prefers? ([^\n]+)") do
        table.insert(facts, "- " .. pref)
    end
    for pref in summary:gmatch("constraint: ([^\n]+)") do
        table.insert(facts, "- " .. pref)
    end
    for pref in summary:gmatch("user wants? ([^\n]+)") do
        table.insert(facts, "- " .. pref)
    end

    if #facts > 0 then
        local section = "\n## Extracted " .. os.date("%Y-%m-%d %H:%M") .. "\n"
        for _, f in ipairs(facts) do
            section = section .. f .. "\n"
        end
        append_memory(ctx, section)
        ctx:log("memory: appended " .. #facts .. " facts from compaction")
    end

    return nil
end

return M
```

### How it works

```
┌─────────────────────────────────────────────────┐
│ Session 1                                        │
│                                                   │
│  User: "I use NixOS, always use nix-shell"       │
│  Agent: does work, edits files...                 │
│  User: "prefer functional style, no unwrap()"     │
│  Agent: does work...                              │
│                                                   │
│  ══ compaction triggers ══                        │
│                                                   │
│  on_compact hook fires:                           │
│    reads conversation summary                     │
│    extracts: "NixOS user", "functional Rust"      │
│    appends to .oneloop/memory.md                  │
│                                                   │
│  Session 2 starts (compacted)                     │
│                                                   │
│  before_api_call hook fires:                      │
│    reads .oneloop/memory.md                       │
│    injects into system prompt                     │
│                                                   │
│  Agent "remembers" preferences                    │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│ Next day — entirely new session                   │
│                                                   │
│  on_start hook fires:                             │
│    reads .oneloop/memory.md                       │
│                                                   │
│  before_api_call hook fires:                      │
│    injects memory into system prompt              │
│                                                   │
│  Agent knows your preferences and project context │
│  Picks up right where you left off                │
└─────────────────────────────────────────────────┘
```

The memory file is plain markdown — readable, editable by the user, and version-controllable:

```markdown
# Project Memory

## User Preferences
_(collected automatically from conversations)_

## Architecture Notes
_(key files, patterns, decisions)_

## Current Work
_(what's in progress)_

## Key Decisions
_(why things were done a certain way)_

## Extracted 2026-04-20 14:32
- Uses NixOS — always nix-shell -p <pkg> for tools
- Prefers functional style Rust (iterator chains over loops)
- No unwrap() outside tests, use .context()?

## Extracted 2026-04-21 09:15
- Auth flow is in src/auth.rs (oneloop login)
- Chose Lua over WASM for plugins (lightweight, REPL-friendly)
```

---

## Example: Safety Plugin

Intercepts tool calls before execution. Blocks dangerous commands, warns on writes outside the project directory, requires confirmation on destructive operations.

```lua
-- .oneloop/plugins/safety.lua

local M = {}
M.hooks = { "before_tool" }

-- Patterns that are ALWAYS blocked
local BLOCKED_PATTERNS = {
    { pattern = "rm%s+%-rf%s+/",           reason = "recursive root delete" },
    { pattern = "rm%s+%-rf%s+~",           reason = "recursive home delete" },
    { pattern = ":(){ :|:& };:",            reason = "fork bomb" },
    { pattern = "dd%s+if=.*of=/dev/",       reason = "raw device write" },
    { pattern = "mkfs%.?",                  reason = "filesystem format" },
    { pattern = ">%/dev/sd",                reason = "raw device write" },
    { pattern = "chmod%s+%-R%s+777%s+/",    reason = "recursive world-writable root" },
}

-- Patterns that require user confirmation
local CONFIRM_PATTERNS = {
    { pattern = "DROP%s+TABLE",             reason = "destructive SQL" },
    { pattern = "TRUNCATE",                 reason = "destructive SQL" },
    { pattern = "DELETE%s+FROM",            reason = "destructive SQL" },
    { pattern = "git%s+push%s+%-%-force",   reason = "force push" },
    { pattern = "git%s+reset%s+%-%-hard",   reason = "hard reset" },
    { pattern = "rm%s+%-rf",               reason = "recursive force delete" },
    { pattern = "cargo%s+publish",          reason = "publishing to crates.io" },
    { pattern = "npm%s+publish",            reason = "publishing to npm" },
}

-- Files that should never be read (leak secrets)
local SENSITIVE_FILES = {
    "id_rsa", "id_ed25519", "id_ecdsa",
    ".env$", ".env%.",
    "credentials%.json",
    "service%-account%.json",
    "%.npmrc", "%.pypirc",
}

local function is_outside_project(path, ctx)
    local abs = ctx:resolve_path(path)
    return not abs:startswith(ctx.project_root)
end

function M.before_tool(tool_name, arguments, ctx)
    -- ── Bash safety ──────────────────────────────
    if tool_name == "bash" then
        local cmd = arguments.command or ""

        for _, rule in ipairs(BLOCKED_PATTERNS) do
            if cmd:match(rule.pattern) then
                ctx:log("safety: blocked bash command (" .. rule.reason .. ")")
                return {
                    action = "block",
                    message = "⛔ Blocked: " .. rule.reason .. ". Command: " .. cmd
                }
            end
        end

        for _, rule in ipairs(CONFIRM_PATTERNS) do
            if cmd:match(rule.pattern) then
                ctx:log("safety: confirmation required (" .. rule.reason .. ")")
                return {
                    action = "confirm",
                    message = "⚠ This command looks destructive ("
                        .. rule.reason .. "):\n  " .. cmd
                }
            end
        end
    end

    -- ── Read safety ──────────────────────────────
    if tool_name == "read" then
        local path = arguments.path or ""
        for _, pattern in ipairs(SENSITIVE_FILES) do
            if path:match(pattern) then
                ctx:log("safety: blocked read of sensitive file: " .. path)
                return {
                    action = "block",
                    message = "⛔ Blocked read: file may contain secrets (" .. path .. ")"
                }
            end
        end
    end

    -- ── Write/Edit safety ────────────────────────
    if tool_name == "write" or tool_name == "edit" then
        local path = arguments.path or ""

        if is_outside_project(path, ctx) then
            return {
                action = "confirm",
                message = "⚠ Writing outside project directory: " .. path
            }
        end

        if path:match("%.oneloop/plugins/") then
            return {
                action = "block",
                message = "⛔ Blocked: cannot modify plugins while running"
            }
        end
    end

    return nil
end

return M
```

---

## Implementation Plan

1. **Add `mlua` dependency** — `Cargo.toml` with `lua54` + `vendored` + `serde` features
2. **Create `src/plugin.rs`** — `PluginManager` struct, plugin discovery, hook dispatch
3. **Define `ctx` API** — Expose `shell`, `read_file`, `write_file`, `append_file`, `file_exists`, `resolve_path`, `project_root`, `log` as Lua functions
4. **Wire hooks into `agent.rs`** — Call `plugins.fire_hook()` at each extension point
5. **Handle `before_tool` return values** — Map `{action="block"}` / `{action="confirm"}` to Rust behavior
6. **Add `/reload` command** — In `app.rs`, re-read plugin files from disk, replace in `Agent`
7. **Ship `memory.lua` and `safety.lua`** as built-in (but overridable) defaults in `~/.oneloop/plugins/`
8. **Add `plugins` field to `Agent`** — Load on startup, pass through to hook points
9. **Update `docs/architecture.md`** — Add plugin system to source layout
