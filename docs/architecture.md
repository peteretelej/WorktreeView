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
  `list_refs`, `list_commits`, `list_review_changes`, `list_surfaces`,
  `read_review_patch`.
- `git/exec.rs`: spawns Git with explicit argument arrays, bounded output
  (4 MiB per stream), a deadline (10 seconds for local probes, 60 for the
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
  refs) from Git results.
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

A flat React + Vite app: `App.tsx` (shell, project overview, and review
views), `settings.tsx`, `diff.ts` (diff presentation helpers),
`highlight.ts` (progressive diff token highlighting over Shiki),
`navigation.ts` (back and forward review history), `reviewPresets.ts`
(review base and worktree scope preset helpers), and `surfaces.ts` (surface
pin and archive helpers). Unit tests for the utilities run with
`npm run test:unit`.

## Persistence

SQLite stores repositories, pin order, settings, and retrospected surface
identities. It also caches commit history: `list_commits` resolves the start
ref with one fresh `git rev-parse`, then serves log pages and default-base
ancestry marks from the cache when the resolved SHAs match what was fetched
before. Pages key on `(repo_path, start_sha, against_sha, skip, limit)` with
commit rows keyed by SHA, and ancestry marks on `(commit_sha, against_sha)`;
a branch move changes the resolved SHA, so stale pages are never served.
Cache writes are best-effort and never fail an open.

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
binary does not know fails startup with a message instead, untouched.
Migrations are append-only
(CONTRIBUTING.md), so divergence is not expected on normal upgrades; desktop
e2e containers rebuild their database on every run. Review flows remain
read-only; the refresh action's fetch is the one Git write, and it touches
remote-tracking refs only (see
[safety-model.md](safety-model.md)).
