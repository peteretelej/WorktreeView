# Agent submissions

Agents collaborate with WorktreeView over one loopback listener with two
faces: the stateless MCP face at `/mcp` (discovery, reads, comment
collaboration, refresh ping) and the raw JSON-RPC face at `/` for
delivering review submissions. This page is the client contract for both:
the submission schema, the delivery transport (authentication, discovery
file, the `post_review` and `refresh_repo` methods, arrival cue), and the
MCP endpoint with its tool surface.

## Submission schema

A submission is a JSON object against a review identity. Unknown fields are
rejected anywhere in the payload (strict schema).

| Field | Type | Rules |
| --- | --- | --- |
| `agent_name` | string | required, non-empty, at most 200 characters |
| `agent_model` | string | required, non-empty, at most 200 characters |
| `command_context` | string or null | optional, at most 500 characters |
| `sections` | array | at most 32 entries, see [Sections](#sections) |
| `findings` | array | at most 200 entries, see [Findings](#findings) |

Size caps: the whole serialized payload is at most 2 MiB; any `html`
section body is at most 1 MiB; every other section body and every finding
body is at most 256 KiB. The ingest cap is authoritative: the transport
adds only a coarser pre-parse guard, so a submission accepted here is never
size-rejected by the transport.

## Sections

A section is `{ "kind": string, "title": string, "body": string }`. The
kind vocabulary is app-owned; the body carries the content.

- `brief`, `walkthrough`, `notes`: markdown bodies, rendered through the
  app's sanitized markdown renderer (GFM extensions such as tables, task
  lists, and strikethrough render; scripts and raw HTML never reach the
  DOM).
- `html`: static client HTML, rendered in a sandboxed iframe. The sandbox
  denies scripts and forms and gives the document an opaque origin; the
  app's Content Security Policy applies to the frame, so external fetches
  are blocked (`default-src 'self'`; data images render under
  `img-src 'self' data:`). The frame is visibly badged as client content.
- Any other kind: stored as sent and rendered through the same safe
  generic card under its raw kind label. Unknown kinds are never rejected;
  they keep submissions forward-compatible.

## Findings

A finding is `{ "title": string, "body": string, "file": string?, "start":
integer?, "end": integer?, "priority": string }`. The title is required
(non-empty, at most 500 characters) and validated as part of the schema;
the stored comment body is the finding body. The priority must be `P0`,
`P1`, `P2`, or `P3` and becomes the comment's severity label.

Findings do not duplicate into a separate store: each one materializes as
an ordinary comment authored by the agent (name and model from the
submission), tagged with its priority and a reference to the submission,
in the review's merged comment stream. Ingested findings are also
attributed to the calling token, so the agent owns its comments: only
that token (or a human) can later edit or delete them. The submission's
`agent_name` stays the display name; it is not tied to the token.

Anchors mirror human comments:

- no `file`: review-level;
- `file` without `start`: file-level;
- `file` with `start` (and optional `end`): line-anchored. Findings carry
  no side field: line anchors are always the RIGHT (new) side.

A malformed anchor (a line number without a file, an end before its start,
a line 0) is rejected.

## Rejection

The ingest validates schema, vocabulary, and caps before writing anything,
and the submission plus all finding comments commit atomically. Violations
are rejected with one of two error codes:

- `invalid_submission`: any submission-shape violation (unknown field,
  missing or empty required string, cap breach, unknown priority,
  malformed anchor);
- `unknown_review_target`: the `repo_path` has no row in the store, so
  there is no review to attach to.

## Rendering

The review surface lists each submission in a reviews strip as an
expandable agent card (name, model, timestamp, command context) holding
its section cards. Finding comments render inline and in the stream like
human comments, with their P0-P3 severity badge; the author filter
separates agent authors.

## Delivery

Agents deliver submissions over JSON-RPC 2.0 to an HTTP endpoint served
inside the WorktreeView process. The listener binds the address and port
configured in the Settings Agent API section (loopback `127.0.0.1:9888`
by default) when the app starts; nothing is reachable from outside the
machine unless the listen address is changed there. A bind failure, such
as a port already in use, never blocks app startup: the Settings section
shows the error and no discovery file is written. The endpoint serves
axum HTTP semantics over a raw tokio connection loop; request heads are
parsed with httparse, hyper's own parser, because hyper's h1 connection
layer does not deliver responses on the current Windows host
(upstream-report candidate).

### Authentication

Every request carries `Authorization: Bearer <token>`. Tokens are
per-agent rows minted in the Settings Agent API section: the secret is 32
random bytes hex-encoded, shown once at creation, and only its SHA-256
hash is stored. A token's activity is recorded as a last-used timestamp,
so agent calls are attributable. Revoking a token refuses it immediately;
the endpoint answers a missing, wrong, or revoked secret identically.

One designated default token is provisioned fresh at every app start
(exactly like the per-boot secret this token model replaced): its secret
goes only into the discovery file, so zero-config discovery clients keep
working across restarts without any secret persisting. The default token
cannot be revoked for the current boot; it rotates at the next start.
Named tokens persist across restarts.

### Discovery

After a successful bind the app writes `agent-endpoint.json` into its app
data directory (Linux `$XDG_DATA_HOME/com.etelej.worktreeview`, macOS
`~/Library/Application Support/com.etelej.worktreeview`, Windows
`%APPDATA%\com.etelej.worktreeview`):

```json
{ "port": 54321, "token": "<64 lowercase hex chars>" }
```

The token is the current boot's default agent token: 32 random bytes
hex-encoded, generated fresh per boot and never persisted across
restarts. When the endpoint is disabled in Settings, or its bind fails,
no discovery file is written or refreshed. Debug builds write
`agent-endpoint-dev.json` instead, so a dev instance and an installed
release never claim each other's registration. Clients read the discovery
file on startup and re-read it whenever the endpoint refuses their token.
Parallel dev instances (separate worktree checkouts) share that dev
discovery file and the last-started instance owns it, so a submission can
land in a different checkout's instance; per-checkout isolation of the
registration is not part of this contract yet.

### Calling post_review

POST to `http://127.0.0.1:<port>/` with an `Authorization: Bearer <token>`
header and a JSON-RPC 2.0 request whose `params` combine the review
identity fields with the submission schema above:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "post_review",
  "params": {
    "repo_path": "/repos/demo",
    "base_sha": "3f2a...",
    "target_key": "/repos/demo",
    "target_kind": "worktree",
    "agent_name": "my-agent",
    "agent_model": "model-id",
    "command_context": null,
    "sections": [{ "kind": "brief", "title": "Summary", "body": "All good." }],
    "findings": []
  }
}
```

`target_kind` is `"worktree"` (`target_key` is then the worktree path) or
`"head"` (`target_key` is then the resolved target SHA). `base_sha` is the
resolved base the review keys on. Unknown fields are rejected anywhere in
`params`, as in the schema.

Delivery is synchronous: a successful response returns the submission id
after the submission and its finding comments are stored, and the app
announces the arrival with a visible cue naming the agent.

```json
{ "jsonrpc": "2.0", "id": 1, "result": { "submission_id": 12 } }
```

### Calling refresh_repo

The endpoint accepts a second method, `refresh_repo`: a ping that asks
the app to refresh one open repository's remote-tracking refs. The ping
reuses the app's own refresh path unchanged (remote-tracking refs only,
a 60 second deadline, bounded output) and the app owns the operation;
agents never drive Git themselves.

POST to the same URL with the same authorization header and strict
params: unknown fields are rejected.

```json
{ "jsonrpc": "2.0", "id": 2, "method": "refresh_repo", "params": { "repo_path": "/repos/demo" } }
```

`repo_path` must match an open repository exactly as the app displays it.
A successful fetch, including the no-remote no-op, answers:

```json
{ "jsonrpc": "2.0", "id": 2, "result": { "ok": true } }
```

A fetch failure (offline, rejected credentials) returns `-32603` with the
Git error message; an unknown `repo_path` returns `-32002`. The call is
attributable like any other request: it requires a valid token and its
use is recorded on the token row.

### Errors

Error responses carry `{ "code", "message" }`:

| Code | Meaning |
| --- | --- |
| `-32700` | the body is not valid JSON |
| `-32600` | the request is not JSON-RPC 2.0 |
| `-32601` | method other than `post_review` or `refresh_repo` |
| `-32602` | invalid params or submission-shape violation (store message passes through) |
| `-32603` | internal error, such as a storage or fetch failure |
| `-32001` | missing, wrong, or revoked bearer token |
| `-32002` | unknown review target: `repo_path` has no open repository |
| `-32003` | request body exceeds the 3 MiB transport guard |

HTTP status mirrors the transport-level outcomes: 401 for auth failures
and 413 for oversized bodies, both with a JSON-RPC body; non-POST methods
get 405 and unknown paths 404 with short plain-text bodies. All
JSON-RPC-framed outcomes otherwise use 200.

### Size caps and stale tokens

The schema's ingest caps are authoritative; the transport adds only a
coarse 3 MiB pre-parse guard, so a schema-legal submission is never
transport-rejected. A 401 means the presented secret is missing, wrong,
or revoked. Discovery clients should first re-read the discovery file:
each boot rotates the default token, and another instance may own the
registration. Agents using a named token paste a fresh one from the
Settings Agent API section; a revoked token is refused until replaced.
Connection refusal simply means the app is not running.

## The MCP face at /mcp

The same listener serves a stateless MCP (Model Context Protocol) face at
`POST /mcp`: the full agent tool surface over the same shared
implementation the raw face uses. Authentication is the same single
listener evaluation (same bearer tokens, same discovery file, same
`401`/`-32001` shape); there is no second auth layer. The face implements
the stateless subset of the MCP `2026-07-28` revision plus the
legacy-client shim described under Stateless shape. It is validated
end-to-end with Codex 0.130; OpenCode 1.1.34 connects through the same
shim (its revision predates per-request `_meta` versions).

### Stateless shape

There is no session: every request is independent and nothing is stored
between requests. Tools requests carry their protocol version in params
`_meta`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/list",
  "params": {
    "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" }
  }
}
```

The face supports `2026-07-28`. A request carrying any other explicit
version is refused with `-32022`; a missing `_meta` field is accepted and
treated as `2026-07-28` (clients on older revisions never send it).
Responses are JSON; the face offers no SSE stream and nothing subscribes
(`GET /mcp` answers `405`). Batching is not implemented: a batch request
(a JSON array body) answers with a single `-32600`.

Clients on pre-`2026-07-28` revisions negotiate a session at connect
time; the face answers the handshake statelessly so they can proceed:

- `initialize` answers
  `{ "protocolVersion", "capabilities": { "tools": {} }, "serverInfo" }`.
  A requested `params.protocolVersion` that is a well-formed revision
  string (for example `2024-11-05`, `2025-06-18`, or `2025-11-25`) is
  echoed; an absent or malformed one gets `2026-07-28`. No session id is
  issued.
- `ping` answers the standard empty result `{}`.
- Any notification (a request without an `id`) answers HTTP `202` with an
  empty body and no JSON-RPC response.

### server/discover

`server/discover` is the version probe; it needs no `_meta` version and
answers with what the face supports:

```json
{
  "jsonrpc": "2.0", "id": 1,
  "result": {
    "protocolVersions": ["2026-07-28"],
    "capabilities": { "tools": {} },
    "serverInfo": { "name": "worktreeview", "version": "0.1.0" }
  }
}
```

### tools/list

`tools/list` returns the static tool catalog in deterministic order
(reads, writes, action). Each descriptor carries `name`, `description`,
`inputSchema` (JSON Schema), and the cache fields `ttlMs` (3600000) and
`"cacheScope": "private"`: the tool set is static per boot, so a generous
client-side cache is honest.

### tools/call

`tools/call` takes `{ "name", "arguments" }` and answers a complete
result whose `content` carries the JSON payload as text:

```json
{
  "jsonrpc": "2.0", "id": 2,
  "result": {
    "resultType": "complete",
    "isError": false,
    "content": [{ "type": "text", "text": "[{\"path\": \"/repos/demo\"}]" }],
    "_meta": {
      "io.modelcontextprotocol/serverInfo": { "name": "worktreeview", "version": "0.1.0" }
    }
  }
}
```

Malformed calls never execute: an unknown tool or arguments that fail the
tool's schema answer `-32602`, and an unknown protocol method answers
`-32601`. Once a call executes, every failure is an `isError: true` result
with the failure message in `content`, never a JSON-RPC error: ownership
refusals, unknown repositories, and fetch failures are outcomes of the
call itself. Authentication never appears as a tool result; it stays at
the transport layer (HTTP 401, `-32001`).

### Tools

| Tool | Params (required bold) | Result payload | Execution failures |
| --- | --- | --- | --- |
| `list_repos` | none | array of repo rows: `path`, `name`, `created_at`, `last_opened_at`, `pinned_at` | - |
| `list_review_targets` | `repo_path` | `worktrees` (`path`, `branch`, `head`), `branches` and `remote_branches` (`ref_name`, `head`, `author`, `subject`, `commit_date`, `upstream`, `ahead`, `behind`) | unknown repo |
| `list_comments` | `repo_path`, `base_sha`, `target_key`, `target_kind` | array of stored comments with anchor fields as stored | - |
| `list_submissions` | `repo_path`, `base_sha`, `target_key`, `target_kind` | array of stored submissions with typed `sections` | - |
| `create_comment` | `repo_path`, `base_sha`, `target_key`, `target_kind`, `body`; optional `severity` (`P0`-`P3`), `file_path`, `side` (`LEFT`/`RIGHT`), `start_line`, `end_line` | the stored comment | unknown repo; shape violations (empty body, bad severity, malformed anchor) |
| `reply_comment` | `parent_comment_id`, `body` | the stored reply | unknown comment; replying to a reply |
| `resolve_thread` | `root_comment_id`, `resolved` | the updated root comment | unknown comment; resolving a reply |
| `edit_own_comment` | `comment_id`, `body` | the updated comment | unknown comment; not your comment |
| `delete_own_comment` | `comment_id` | `{ "deleted": true }`; a root delete also removes its replies | unknown comment; not your comment |
| `refresh_repo` | `repo_path` | `{ "ok": true }` | unknown repo; fetch failure |

Reads are find-only: a review identity with no comments yet answers an
empty array and never creates a review row. Writes accept exactly the
inputs the human app accepts: the same validators, the same caps, the same
anchor shapes (no `file` for review-level, `file` only for file-level,
`file` with `side` and `start_line` for line-level). Tool line anchors
carry no line content, so nothing is hashed at write time and drift
matching does not apply to them, exactly like ingested findings.
`list_comments` reports anchor data as stored; it does not recompute drift
against live Git (that runs only on the human review path).

### Identity and ownership

Every MCP call is attributed to its bearer token: created comments record
the token's name and id. An agent edits and deletes only comments authored
through its own token; human comments and legacy comments with no owning
token are never agent-mutable. The refusal is an `isError` result carrying
"Only the agent token that authored a comment can edit or delete it."
Resolving or reopening a thread is visible and reversible, so any agent
may resolve any thread. Threads are one root plus flat replies: only a
root accepts a reply, and only a root can be resolved.

### Reactivity

The app keeps its own UI current when agents act. After a successful
comment mutation the running app refreshes the loaded review's comment
stream, so an agent's create, reply, resolve, edit, or delete becomes
visible to a human without a manual reload; after a completed
`refresh_repo` the open repository's surfaces re-list, so a
push-then-ping shows up the same way. Tool comments render under the
token's agent name with the agent badge, like ingested findings, and the
stream's author filter separates them. This reactivity is app behavior,
not client-facing surface: there is no event stream or subscription for
agents, and the synchronous tool result remains the only thing a client
depends on.

### Conformance notes

- SEP-2243: the `Mcp-Method`/`Mcp-Name` request headers are required on
  Streamable HTTP POSTs by the spec, but they exist for intermediaries.
  This face routes from the JSON body and neither requires nor validates
  them; revisit only if a real client or intermediary is shown to require
  validation.
- The `initialize`/`ping`/notification answers exist only to let
  pre-2026-07-28 clients connect. The face still holds no sessions and
  issues no `Mcp-Session-Id`: a legacy client must tolerate stateless
  per-request connections and must not require an SSE stream (`GET`
  answers `405`).
- Connections are served one at a time and each connection is one request
  with a bounded 30 s stall window: concurrent agent calls queue behind
  each other. Agents doing parallel work should tolerate the latency.
- The endpoint binds the address and port configured in the Settings Agent
  API section (loopback `127.0.0.1:9888` by default); nothing is reachable
  from outside the machine unless the listen address is changed there.
