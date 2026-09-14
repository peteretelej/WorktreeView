# Architecture

WorktreeView is a Tauri 2 desktop app: a React webview for presentation and
interaction, a Rust backend for everything native. Git semantics and
filesystem access stay out of the React layer; the webview receives
normalized domain data through narrow, typed Tauri commands.

## Backend (`src-tauri/src`)

- `commands.rs`: thin typed IPC adapters. The full command surface:
  `open_repo`, `list_repos`, `list_worktrees`, `list_worktree_status`,
  `remove_repo`, `get_branch_inventory`, `fetch_project`,
  `set_repo_pinned`, `set_surface_pinned`, `get_settings`, `set_settings`,
  `list_agent_tokens`, `create_agent_token`, `delete_agent_token`,
  `get_mcp_status`, `restart_mcp`,
  `list_refs`, `list_commits`, `describe_commit`, `list_review_changes`,
  `list_surfaces`, `read_review_patch`, `read_review_file`, `open_review_file`,
  `create_comment`,
  `list_comments`, `list_submissions`, `reply_comment`, `set_comment_resolved`,
  `edit_comment`, `match_comment_anchors`.
- `agents.rs`: the token store: per-agent tokens as table rows with only
  their SHA-256 hex hash persisted. Secrets carry a `wv` prefix followed
  by 32 random bytes hex, generated once at creation and never stored or
  logged; authentication
  hashes the presented secret and matches a non-revoked row, recording a
  last-used timestamp. Deletion is immediate and refuses the current
  default token, which the listener's startup path provisions fresh
  per listener start, boot or restart (deleting the previous default in
  the same transaction) and publishes only through the config file.
- `git/exec.rs`: spawns Git with explicit argument arrays, bounded output
  (16 MiB per stream), a deadline (30 seconds for local probes, 300 for the
  fetch the refresh action runs), and kill-on-drop cancellation.
  Repository-scoped runs share one hardened builder; no-index diffs get
  their own isolated stdin command.
- `git/validate.rs`: path, ref, scope, and flag-combination validation, plus
  repository classification through `git rev-parse --is-inside-work-tree`.
- `git/filters.rs`: detects configured clean and process filters and
  neutralizes them so review data never executes repository-defined
  filters.
- `git/parse.rs`: parses Git porcelain output (name-status, numstat,
  commits, branch records, worktrees, untracked paths) into typed domain
  structs.
- `overview.rs`: assembles the project page's branch inventory from one
  `for-each-ref refs/heads refs/remotes` pass (per-branch head,
  last-commit identity, upstream and its ahead/behind track), the origin
  URL, and a bounded ahead/behind fallback against the default branch for
  local worktree branches without an upstream. Remote-tracking branches
  ride the same pass and skip the fallback probe.
- `portal.rs`: the Pulse portal's store-backed listing of review
  identities. `list_portal_reviews_in_pool` lists every identity carrying
  a review request or any comment/submission activity (including settled
  and request-less ones) as one row per identity with the backend-owned
  state classification (open, settled, stale from the attention queue's
  changed-since-review comparison, or no request), the identity's
  unresolved severity counts, and last-activity ordering under a fixed
  row cap; the optional text needle matches as an ASCII-case-insensitive
  substring. No Git runs on the path.
- `review.rs`: assembles review data (changed files, patches, commits,
  refs) from Git results, plus the new-side file content that context
  expansion and the full-file view render (worktree read or blob at a rev,
  bounded and binary-sniffed like patches).
- `reviews.rs`: owns the comment substrate: review identity
  resolve-or-create, comment storage and threads, write-time anchor hash
  (FNV-1a over marker-free line content) and snippet capture, and the
  drift matcher. `match_comment_anchors` is one batched command that takes
  the loaded file's parsed lines and returns per-comment statuses
  (current, moved with the nearest re-anchored line, outdated); the
  frontend only maps display sides to logical sides and places the
  results. Anchor-shape validation lives here and is authoritative for
  the package. Every mutating comment operation takes an explicit actor
  (`Human` for the IPC commands, `Agent(identity)` at the endpoint), and
  ownership is enforced exactly here in one site: an agent edits or
  deletes only comments carrying its own `author_token_id`, while human
  and legacy unowned comments are never agent-mutable; replies and
  resolve toggles are open to any actor. It also owns the submission
  ingest: one `ingest_submission_in_pool` validates schema, vocabulary,
  and size caps against the [client contract](agent-submissions.md),
  reuses the anchor validator, stores sections as sent, and materializes
  findings as agent comments with severity, submission reference, and the
  calling token's ownership in one transaction. `list_submissions`
  returns stored sections as typed entries, never raw JSON.
- `events.rs`: owns the activity-log SQL: `record_event` takes the
  caller's executor (connection or transaction) so an event commits with
  the mutation it narrates, and `list_events_in_pool` serves both order
  directions under a fixed row cap with the `since_id` cursor and
  `repo_path` filters. Mutation modules call it from their own production
  mutation functions; no emission lives in a handler layer or announce
  sink.
- `requests.rs`: the review-request store and lifecycle engine over the
  `review_requests` table: strict schema validation (note cap, lens
  vocabulary, reviewer token names, 1-3 round budget), the state machine
  with sticky verdicts (the first blocking verdict in a round wins and
  only the requester's re-request resets it) and full human parity (the
  human actor performs any reviewer transition, never gated by a request's
  named reviewers list), same-requester dedup on (identity, requester,
  head), and the round budget whose exhaustion leaves the request for the
  human. The P0/P1 blocking definition lives once here as a constant: the
  submission ingest computes its observation flag from it, while the
  attention and triage queries restate `P0`/`P1` inline in SQL (same
  vocabulary, two spellings). A submission observed through the ingest
  implicitly claims open requests on the identity and applies the
  verdict, except the requester's own submissions, which never settle
  their own request; a submitter's token id gates that check. Status
  transitions carry their status precondition in the UPDATE, so racing
  writers resolve first-writer-wins. Mutations announce through the
  injected `RequestChangeSink` exactly once per successful mutation.
- `transport.rs`: the agent endpoint, the app's one inbound network
  surface. A `TcpListener` binds the address and port configured in
  Settings (loopback `127.0.0.1:9888` by default) inside the app process;
  a bind failure is a normal condition that updates a shared status
  handle the `get_mcp_status` command reads and the Settings Agent API
  section shows, never a startup failure, and it writes no config
  file or default token for a dead endpoint. Bearer authentication is
  evaluated only here, once per request, against the `agent_tokens`
  table; the resolved identity flows into every handler. Requests and
  responses use axum's HTTP semantics over a raw tokio connection loop
  whose request heads are parsed with httparse (hyper's own parser);
  hyper's h1 connection layer is bypassed because it does not deliver
  responses on the current Windows host (upstream-report candidate). It
  owns transport concerns only: bearer auth, JSON-RPC 2.0 framing with
  the documented error-code matrix, and a coarse 3 MiB pre-parse body
  guard. Connections are served serially; heads are capped at 64 KiB and
  64 headers, and a stalled connection is dropped after 30 s. The methods
  `post_review` and `refresh_repo` delegate to the shared implementations
  (`ingest_submission_in_pool`, `refresh_repo`) with the authenticated
  actor; after a successful ingest the endpoint pushes a
  `submission-received` event, after a successful refresh (including
  the no-remote no-op, which the shared path announces) a
  `project-refreshed` event, and after each successful agent comment
  mutation on the MCP face a `comment-changed` event, all via injected
  sinks so the handler matrix is testable without an app; the webview
  never listens on a socket. On a successful bind the startup path
  provisions the default token and writes the config file;
  exit shutdown is best-effort and removes the file only when still
  owned. A stale config file may be left behind; clients tolerate
  that by re-reading the file when their token is refused. The
  `restart_mcp` command (the Settings Agent API Restart action) stops
  the running listener and waits for its thread, freeing the socket,
  then binds a fresh one from the settings as persisted right now
  through the same start path and the same shared status handle; the
  default token renews on every start.
- `cache.rs`: SQLite-backed history cache for commit pages and ancestry
  marks, keyed by resolved SHAs.
- `retrospection.rs`: records reviewed worktree and branch identities
  (last resolved head, last-seen time) at open time and lists surfaces that
  have disappeared from the live worktree and ref inventory. It also owns
  per-surface pins: pinning a recorded surface updates its row in place,
  and pinning a never-reviewed surface resolves its identity with one
  bounded Git spawn before recording it with pin origin.
- `store.rs`: SQLite persistence (sqlx) for repositories, pins, and
  settings, plus path canonicalization and normalization of stored
  Windows verbatim paths. Migrations live in `src-tauri/migrations`.

## Git as the semantic authority

The native Git CLI is the only Git engine; there is no libgit2. The CLI
matches each user's installed Git exactly (repo formats, ref semantics,
diff behavior), streams output that can be bounded and cancelled, and keeps
Git crashes in a child process instead of the app. Queries use stable
porcelain formats, and invocations whose stderr is machine-parsed pin the
locale to C.

## Frontend (`src/`)

A flat React + Vite app: `App.tsx` (shell, project overview, state and
effects, and the agent submission arrival cue and endpoint event listeners:
`submission-received` queues the arrival cue, `comment-changed` refetches
the loaded review's comments when the change names it, and
`project-refreshed` re-lists the open repository's surfaces),
`format.ts` (shared formatting and error-message helpers),
`ui.tsx` (shared widgets: copy button, empty state, pager, brand mark,
top bar), `review.tsx` (the review surface: review view, changed-file
tree and diff panes, ref picker, review request bar and forms),
`history.tsx` (the commit-history quick look),
`settings.tsx`,
`diff.ts` (diff presentation helpers),
`highlight.ts` (progressive diff token highlighting over Shiki),
`navigation.ts` (back and forward review history), `reviewPresets.ts`
(review base and worktree scope preset helpers), `surfaces.ts` (surface
pin and archive helpers), `comments.ts` (comment mirror types and
logical-side helpers) with `comments.tsx` (comment hook, stream panel,
and inline composers), `canvas.ts` (submission mirror types and section
view mapping) with `canvas.tsx` (section cards, the sandboxed html block,
and the reviews strip), and `markdown.tsx` rendering comment bodies through
`react-markdown` with `rehype-sanitize`'s default schema
(`markdown-schema.ts` keeps the schema constant unit-testable). Unit tests
for the utilities run with `npm run test:unit`.

## Persistence

SQLite stores repositories, pin order, settings, retrospected surface
identities, review sessions with their comments and agent submissions,
review requests, plus a commit history cache: `list_commits` resolves the start
ref with one fresh `git rev-parse`, then serves log pages and default-base
ancestry marks from the cache when the resolved SHAs match what was fetched
before. Pages key on `(repo_path, start_sha, against_sha, skip, limit)` with
commit rows keyed by SHA, and ancestry marks on `(commit_sha, against_sha)`;
a branch move changes the resolved SHA, so stale pages are never served.
Cache writes are best-effort and never fail an open.

Reviews key on `(repo_path, base_sha, target_key, target_kind)` where the
SHAs are resolved (the worktree path for live worktree targets), so a moved
branch naturally starts a new comment session. Comments reference their
review, optionally a parent comment (one root plus flat replies), and
optionally a file or logical line range; line comments carry the write-time
anchor hash and bounded snippet that drift detection needs later.
Submissions reference their review and store the agent identity and their
sections as JSON; findings are never stored separately, they are comment
rows authored by the agent with a severity and a submission reference.
Comments may carry an `author_token_id` naming the agent token that owns
them. All three tables cascade from the `repos` row, as do the older
repo-scoped caches. Review requests key on the same review identity and
cascade from the `repos` row too; their requester token reference is set
null when the token is deleted, which makes the request human-keyed from
then on. The schema is one consolidated `0001` migration plus
append-only additive migrations (`0002` adds the `agent_tokens` table and
comment ownership; `0003` adds review requests; `0004` adds the `events`
table); migration divergence handling is described at the end of this
section.

The `events` table is the append-only activity log: every review-relevant
mutation (request lifecycle, submission delivery, comment posts and
resolutions, surface head moves, repository registrations) writes one row
at the shared store-layer mutation function, so human IPC and agent
endpoint paths narrate identically and emission never sits in a handler
layer. Rows carry the review identity when one exists (nullable
`base_sha`/`target_key`/`target_kind`, `request_id`, `comment_id`), the
actor (`actor_kind`/`actor_name`), a short human-readable `summary`, and
a closed `kind` vocabulary enforced by CHECK. `repo_path` carries no
foreign key by design: removing a repository cascades its reviews and
requests but the event narration survives in the activity feed. Events
are never deleted or rewritten (no edit or deletion kinds exist), there
is no backfill (the feed starts when emission activates), and retention
is deferred until volume demands it.

Retrospected surfaces key on `(repo_path, kind, identity_key)` and carry the
recorded label, head, pin state, and row origin (`review` for recorded
opens, `pin` for surfaces created by pinning). The repo-scoped tables
(`log_pages`, `retrospected_surfaces`) declare
`REFERENCES repos(path) ON DELETE CASCADE`, so deleting a repo's row prunes
its cached pages and surface rows automatically; the shared `commits` and
`ancestry_marks` content is keyed by SHA and is never cascaded. Pruning is a
store-level property: the remove-project flow (`remove_repo`) deletes the
registry row and the cascade prunes that repo's cache and retrospection
rows; nothing on disk is touched.

Opening a worktree's or branch's history (or its review) records the
surface's identity and resolved head in `retrospected_surfaces`; recording
runs only after the ref resolves, so only surfaces that exist get recorded,
and it never records raw-SHA, tag, or rev-expression opens. `list_surfaces`
compares recorded identities against the live `worktree list` and
`for-each-ref` inventory on every call and reports surfaces that are no
longer present; nothing about the live inventory is cached. A gone surface's
history reopens from the cache, or re-derives from Git while the objects
exist; when Git can no longer resolve a recorded head, `commit_page`
degrades to `content_unavailable` instead of evicting anything.
Migration handling is non-destructive: when the embedded migration set
diverges from a store (as after the 0.0.1 consolidation), startup moves the
store files to `worktreeview.sqlite3.bak`, overwriting any previous backup,
and rebuilds the store; a store recording a migration version the running
binary does not know also keeps its files untouched, but startup asks
whether to set it aside as the same backup and rebuild, or to quit so the
app can be updated instead. Everything lives in one app home under the
user profile, `~/.worktreeview/` (the store, `config.json` carrying
the endpoint's `{port, token}`, and the published agent skill under
`skills/worktreeview/`, embedded in the binary and refreshed on every
launch so the copy always matches the running app), so agents and
the Settings UI find it at the same documented path on every OS. Debug
builds (`npm run dev`) use `~/.worktreeview-dev/` instead and keep their
own store per checkout, named from the checkout's target directory
(`worktreeview-dev-<label>-<hash>.sqlite3`); installed releases use
`worktreeview.sqlite3`, and the separate home pairs each channel's
registration the same way unless a `WORKTREEVIEW_DATA_DIR` or `--home`
override points both channels at the same directory.
`WORKTREEVIEW_DATA_DIR` or a `--home <dir>` launch argument relocates the
home. A first run after an upgrade copies the store once from the old
per-OS app data directory, leaving it as a backup. Migrations are
append-only
(CONTRIBUTING.md), so divergence is not expected on normal upgrades; desktop
e2e containers rebuild their database on every run. Review flows remain
read-only; the refresh action's fetch is the one Git write, and it touches
remote-tracking refs only. Comment and submission data lives only in the
app store, so a set-aside backup retains it. See
[safety-model.md](safety-model.md).
