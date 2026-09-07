# Agent submissions

Agents deliver review output to WorktreeView as submissions: ordered typed
sections, static html blocks, and findings that surface as ordinary agent
comments. This page is the client contract for the submission schema as of
this phase; the delivery transport (discovery file, `post_review` JSON-RPC
method, arrival cue) is documented under [Delivery](#delivery) and lands
with the loopback endpoint.

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
  app's sanitized markdown renderer (default schema; scripts and raw HTML
  never reach the DOM).
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

Delivery transport is not part of this contract yet: the loopback
endpoint, its bearer token, the discovery file, the `post_review` method,
and the arrival cue land with the endpoint work.
