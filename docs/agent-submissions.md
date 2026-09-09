# Agent submissions

Agents deliver review output to WorktreeView as submissions: ordered typed
sections, static html blocks, and findings that surface as ordinary agent
comments. This page is the client contract for the submission schema and
the delivery transport (discovery file, `post_review` JSON-RPC method,
arrival cue) as of this phase.

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
in the review's merged comment stream.

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
inside the WorktreeView process. The listener binds `127.0.0.1` on an
ephemeral port; nothing is reachable from outside the machine. The
endpoint serves axum HTTP semantics over a raw tokio connection loop;
request heads are parsed with httparse, hyper's own parser, because
hyper's h1 connection layer does not deliver responses on the current
Windows host (upstream-report candidate).

### Discovery

After a successful bind the app writes `agent-endpoint.json` into its app
data directory (Linux `$XDG_DATA_HOME/com.etelej.worktreeview`, macOS
`~/Library/Application Support/com.etelej.worktreeview`, Windows
`%APPDATA%\com.etelej.worktreeview`):

```json
{ "port": 54321, "token": "<64 lowercase hex chars>" }
```

The token is 32 random bytes hex-encoded, generated fresh per boot and
never persisted across restarts. Debug builds write
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

### Errors

Error responses carry `{ "code", "message" }`:

| Code | Meaning |
| --- | --- |
| `-32700` | the body is not valid JSON |
| `-32600` | the request is not JSON-RPC 2.0 |
| `-32601` | method other than `post_review` (the endpoint is write-only v0) |
| `-32602` | invalid params or submission-shape violation (store message passes through) |
| `-32603` | internal error, such as a storage failure |
| `-32001` | missing or wrong bearer token |
| `-32002` | unknown review target: `repo_path` has no open repository |
| `-32003` | request body exceeds the 3 MiB transport guard |

HTTP status mirrors the transport-level outcomes: 401 for auth failures
and 413 for oversized bodies, both with a JSON-RPC body; non-POST methods
get 405 and unknown paths 404 with short plain-text bodies. All
JSON-RPC-framed outcomes otherwise use 200.

### Size caps and stale tokens

The schema's ingest caps are authoritative; the transport adds only a
coarse 3 MiB pre-parse guard, so a schema-legal submission is never
transport-rejected. A 401 means the token no longer matches this boot:
the app restarted or another instance owns the discovery file. Re-read
the discovery file and retry with the fresh token; connection refusal
simply means the app is not running.
