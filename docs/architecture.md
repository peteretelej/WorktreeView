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
  `list_agent_tokens`, `create_agent_token`, `revoke_agent_token`,
  `get_mcp_status`,
  `list_refs`, `list_commits`, `describe_commit`, `list_review_changes`,
  `list_surfaces`, `read_review_patch`, `read_review_file`, `open_review_file`,
  `create_comment`,
  `list_comments`, `list_submissions`, `reply_comment`, `set_comment_resolved`,
  `edit_comment`, `match_comment_anchors`.
- `agents.rs`: the token store: per-agent tokens as table rows with only
  their SHA-256 hex hash persisted. Secrets are 32 random bytes hex,
  generated once at creation and never stored or logged; authentication
  hashes the presented secret and matches a non-revoked row, recording a
  last-used timestamp. Revocation is immediate and refuses the current
  boot's default token, which the listener's startup path provisions fresh
  per boot (revoking the previous default in the same transaction) and
  publishes only through the discovery file.
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
- `transport.rs`: the agent endpoint, the app's one inbound network
  surface. A `TcpListener` binds the address and port configured in
  Settings (loopback `127.0.0.1:9888` by default) inside the app process;
  a bind failure is a normal condition that updates a shared status
  handle the `get_mcp_status` command reads and the Settings Agent API
  section shows, never a startup failure, and it writes no discovery
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
  `submission-received` event, and after a successful refresh (including
  the no-remote no-op, which the shared path announces) a
  `project-refreshed` event, both via injected sinks so the handler
  matrix is testable without an app; the webview never listens on a
  socket. On a successful bind the startup path provisions the per-boot
  default token and writes the discovery file; exit shutdown is
  best-effort and removes the file only when still owned. A stale
  discovery file may be left behind; clients tolerate that by re-reading
  the file when their token is refused.
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
locale to C. The full rationale is in the repo `AGENTS.md`.

## Frontend (`src/`)

A flat React + Vite app: `App.tsx` (shell, project overview, review views,
and the agent submission arrival cue), `settings.tsx`,
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
plus a commit history cache: `list_commits` resolves the start
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
repo-scoped caches. The schema is one consolidated `0001` migration plus
append-only additive migrations (`0002` adds the `agent_tokens` table and
comment ownership); a store recorded under an older migration set
diverges from the embedded baseline and is set aside as
`worktreeview.sqlite3.bak` at startup while a fresh store is rebuilt, so
no manual deletion is needed.

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
app can be updated instead. Debug builds (`npm run dev`) keep their own
store per checkout, named from the checkout's target directory
(`worktreeview-dev-<label>-<hash>.sqlite3`); installed releases use
`worktreeview.sqlite3`, and the endpoint discovery file's `-dev` suffix
pairs each channel's registration the same way.
`WORKTREEVIEW_DATA_DIR` relocates the data directory. Migrations are
append-only
(CONTRIBUTING.md), so divergence is not expected on normal upgrades; desktop
e2e containers rebuild their database on every run. Review flows remain
read-only; the refresh action's fetch is the one Git write, and it touches
remote-tracking refs only. Comment and submission data lives only in the
app store, so a set-aside backup retains it. See
[safety-model.md](safety-model.md).
