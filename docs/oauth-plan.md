# OAuth / Subscription Login — Implementation Plan

> **Status: draft for review.** Do not implement until this file is approved and committed.

## Why

Today oneloop only supports API keys. Several providers (Anthropic, OpenAI) allow their
paid subscription users (Claude Pro/Max, ChatGPT Plus) to authenticate via OAuth 2.0 instead
of buying separate API credits. This unlocks inference against subscription quotas at no
additional per-token cost.

---

## Provider-by-provider findings

### 1. Anthropic (Claude Pro/Max subscription)

**Source of truth:** Claude Code CLI — client ID `9d1c250a-e61b-44d9-88ed-5944d1962f5e`
is Anthropic's own first-party OAuth client registered for CLI tools.

> **Warning (same as zot):** reusing this client ID from a third-party tool is against
> Anthropic's ToS and may be revoked. We document it here for informational purposes and
> will need to decide whether to expose it under a clear flag (e.g. `--experimental-oauth`).

#### Flow type
OAuth 2.0 authorization code + PKCE (S256).

#### Endpoints
| Purpose | URL |
|---|---|
| Authorization | `https://claude.ai/oauth/authorize` |
| Token exchange | `https://platform.claude.com/v1/oauth/token` |
| Token refresh | same token URL with `grant_type=refresh_token` |

#### Parameters
| Field | Value |
|---|---|
| `client_id` | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` |
| `scope` | `org:create_api_key user:profile user:inference` |
| `redirect_uri` (browser) | `http://localhost:53692/callback` |
| `redirect_uri` (headless) | `https://console.anthropic.com/oauth/code/callback` |

#### Anthropic-specific quirks (must get all three right or the flow fails)

1. **state = verifier** — the OAuth `state` parameter is set to the PKCE verifier value,
   not a random nonce.
2. **state in token body** — the token-exchange POST must include `"state": "<verifier>"`.
3. **JSON body** — token requests use `Content-Type: application/json`, not
   `application/x-www-form-urlencoded`.
4. **Extra auth arg** — `code=true` must be in the authorization URL query string.

#### PKCE
Random 32-byte buffer → base64url-encode → verifier.  
`SHA-256(verifier)` → base64url-encode → challenge.  
Method: `S256`.

#### Browser vs headless
- **Browser present** (`$DISPLAY` / `$WAYLAND_DISPLAY` set, macOS/Windows): bind a local
  HTTP server on `localhost:53692`, open `claude.ai/oauth/authorize` in the browser, wait
  for the callback at `/callback?code=X&state=Y`, then exchange.
- **Headless** (Docker, SSH without display): use the manual redirect URI
  (`console.anthropic.com/oauth/code/callback`). The browser shows a page with a
  `code#state` token. User pastes it back into the terminal. Parse as `code#state` or
  a full URL with `?code=X&state=Y`.

#### Token exchange (browser flow)
```
POST https://platform.claude.com/v1/oauth/token
Content-Type: application/json

{
  "grant_type": "authorization_code",
  "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "code": "<auth_code>",
  "redirect_uri": "http://localhost:53692/callback",
  "code_verifier": "<pkce_verifier>",
  "state": "<pkce_verifier>"   // Anthropic quirk: state = verifier
}
```

Response: `{ "access_token", "refresh_token", "expires_in", ... }`

#### Token refresh
```
POST https://platform.claude.com/v1/oauth/token
Content-Type: application/json

{
  "grant_type": "refresh_token",
  "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "refresh_token": "<refresh_token>"
}
```

#### Using the token in API requests
When calling `api.anthropic.com/v1/messages` with an OAuth token instead of an API key,
the wire format changes in four ways:

| What | API key mode | OAuth mode |
|---|---|---|
| Auth header | `x-api-key: <key>` | `Authorization: Bearer <access_token>` |
| `anthropic-beta` | provider's choice | **must** include `claude-code-20250219,oauth-2025-04-20` |
| System prompt prefix | none | **must** start with `"You are Claude Code, Anthropic's official CLI for Claude."` |
| Tool names | our names | **must** use Claude Code's canonical casing: `Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob` |

The system prompt identity line and tool-name casing are enforced server-side; diverging from
them causes 403 or 429 errors on the first request. This is why zot hard-codes the identity
string and a tool-name mapping table.

#### Token storage
```json
// ~/.oneloop/auth.json — proposed extension
{
  "anthropic": {
    "type": "oauth",
    "access_token": "...",
    "refresh_token": "...",
    "expiry": "2026-06-01T12:00:00Z"
  }
}
```
Current schema has `type: "api_key"`. OAuth entries use `type: "oauth"` and add
`access_token`, `refresh_token`, and `expiry` fields. The resolver (`resolve_anthropic_api_key`)
must be updated to return the access token when type is oauth, and to refresh automatically
before it expires (60-second safety margin).

---

### 2. OpenAI (ChatGPT Plus subscription — standard API)

**Source of truth:** OpenAI's Codex CLI — client ID `app_EMoamEEZ73f0CkXaXp7hrann`.

> Same ToS caveat as Anthropic.

#### Flow type
OAuth 2.0 authorization code + PKCE (S256).

#### Endpoints
| Purpose | URL |
|---|---|
| Authorization | `https://auth.openai.com/oauth/authorize` |
| Token exchange | `https://auth.openai.com/oauth/token` |

#### Parameters
| Field | Value |
|---|---|
| `client_id` | `app_EMoamEEZ73f0CkXaXp7hrann` |
| `scope` | `openid profile email offline_access` |
| `redirect_uri` | `http://localhost:1455/auth/callback` |

#### Differences from Anthropic
- **Form-encoded body** — token requests use `application/x-www-form-urlencoded`.
- **state is random** — standard random nonce (not the PKCE verifier).
- **state not in token body** — token exchange does not include `state`.
- **Extra auth args** — `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`.
- **OIDC id_token** — the token response includes an `id_token` JWT. The
  `chatgpt_account_id` claim nested at `"https://api.openai.com/auth".chatgpt_account_id`
  inside the JWT payload is needed for the Codex endpoint (see §3 below).

#### Using the token in API requests (standard `api.openai.com`)
`Authorization: Bearer <access_token>` on `api.openai.com/v1/chat/completions`.
No other header changes — structurally identical to API-key mode.

---

### 3. OpenAI Codex (ChatGPT subscription — Codex endpoint)

This is a **separate client** that shares the same OAuth flow as §2 but talks to a
completely different API endpoint operated by chatgpt.com. It uses OpenAI's newer
**Responses API** (not Chat Completions).

#### Endpoint
`POST https://chatgpt.com/backend-api/codex/responses`

#### Required headers
```
Authorization: Bearer <oauth_access_token>
chatgpt-account-id: <id_from_id_token_jwt>
openai-beta: responses=experimental
originator: zot          # or whatever the agent name is
```

#### Request body shape (Responses API)
```json
{
  "model": "o4-mini",
  "store": false,
  "stream": true,
  "instructions": "<system prompt>",
  "input": [
    { "role": "user", "content": [{ "type": "input_text", "text": "..." }] }
  ],
  "tools": [{ "type": "function", "name": "...", "description": "...", "parameters": {...} }],
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "include": ["reasoning.encrypted_content"]
}
```

This is a fundamentally different wire format from Chat Completions. SSE event types are
also different: `response.output_item.added`, `response.output_text.delta`,
`response.function_call_arguments.delta`, `response.completed`, etc.

Implementing this requires a new provider variant in oneloop, separate from the existing
`openai` provider.

#### Reasoning / thinking
The Codex endpoint supports `reasoning.effort` (`"low"/"medium"/"high"`) via a top-level
`"reasoning": { "effort": "..." }` field. Reasoning items must be **replayed verbatim** on
follow-up requests (the server rejects tool-call continuations without the
`encrypted_content` reasoning blob from the prior turn).

---

### 4. Google Gemini

**No OAuth support for consumer subscriptions.**

From zot's own comment in `packages/provider/gemini.go`:
> Auth model: API key only. Google does NOT issue OAuth tokens for consumer
> Gemini Advanced / Google One AI subscriptions; programmatic access requires either
> an AI Studio API key (this client) or Vertex AI / GCP service-account credentials.

For oneloop: Gemini stays API-key only. No OAuth flow to implement.

---

## Proposed changes to oneloop

### auth.rs — extend `AuthFile` schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderCreds {
    ApiKey { key: String },
    OAuth {
        access_token: String,
        refresh_token: Option<String>,
        expiry: Option<DateTime<Utc>>,
    },
}
```

Each provider field in `AuthFile` becomes `Option<ProviderCreds>`.

`resolve_anthropic_api_key()` (or a new `resolve_anthropic_creds()`) must:
1. Load `ProviderCreds` for `anthropic`.
2. If `OAuth`: check expiry, auto-refresh if within 60 seconds of expiry.
3. Return the access token.

### main.rs — extend `login` subcommand

```
ol login anthropic          # browser OAuth if display available, else paste-code
ol login anthropic --manual # always paste-code (headless/SSH)
ol login openai             # browser OAuth
ol login openai-codex       # same OAuth as openai, selects Codex endpoint
ol login openai --manual    # paste-code fallback
```

The login flow:
1. Generate PKCE pair.
2. Build authorization URL.
3. Detect browser availability (`$DISPLAY`, `$WAYLAND_DISPLAY`, OS).
4. Browser mode: spawn local HTTP server on the fixed callback port, open browser, await callback.
5. Headless mode: print URL, read pasted code from stdin.
6. Exchange code for tokens.
7. Store tokens in `~/.oneloop/auth.json`.

### providers/anthropic.rs — OAuth request mode

When credentials are `OAuth`:
- Use `Authorization: Bearer <token>` instead of `x-api-key`.
- Add `anthropic-beta: claude-code-20250219,oauth-2025-04-20` to every request.
- Prefix system prompt with `"You are Claude Code, Anthropic's official CLI for Claude.\n"`.
- Map tool names to Claude Code canonical casing before sending.

### providers/openai.rs — minimal change

OAuth access token is passed the same way as an API key in the `Authorization: Bearer`
header. The provider struct does not need structural changes.

### providers/openai_codex.rs — new file

New `CodexProvider` using the Responses API wire format described in §3. Shares the OAuth
token and account ID from the OpenAI login flow.

---

## Rollout sequence (suggested)

1. **auth.rs** — extend schema, add PKCE helpers, token refresh logic.
2. **login (Anthropic browser)** — local callback server, open browser, exchange.
3. **login (Anthropic headless)** — paste-code path.
4. **anthropic.rs** — OAuth request mode (headers, identity, tool names).
5. **login (OpenAI)** — browser + headless.
6. **openai.rs** — confirm Bearer auth works (likely already does).
7. **openai_codex.rs** — new Codex provider, Responses API.
8. **Token refresh** — wire into resolver so expiry is handled transparently.

---

## Open questions

- **ToS**: Using Claude Code's and Codex CLI's client IDs from a third-party tool is
  explicitly against Anthropic's and OpenAI's ToS. Zot ships it under `--experimental-oauth`.
  Do we want to do the same, or apply for our own client registration first?
- **Anthropic beta flags**: The `claude-code-20250219` and `oauth-2025-04-20` beta tags are
  required today. They may change; we'll need to track Anthropic's changelog.
- **Codex wire format stability**: The Responses API at `chatgpt.com/backend-api/codex/responses`
  is marked `responses=experimental`. OpenAI may break it without notice.
- **Token storage permissions**: `~/.oneloop/auth.json` should be mode `0600` once OAuth tokens
  (which are more sensitive than API keys the user typed in) are stored there.
