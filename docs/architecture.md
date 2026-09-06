# Architecture

WorktreeView is a Tauri 2 desktop app: a React webview for presentation and
interaction, a Rust backend for everything native. Git semantics and
filesystem access stay out of the React layer; the webview receives
normalized domain data through narrow, typed Tauri commands.

## Backend (`src-tauri/src`)

- `commands.rs`: thin typed IPC adapters. The full command surface:
  `open_repo`, `list_repos`, `list_worktrees`, `set_repo_pinned`,
  `get_settings`, `set_settings`, `list_refs`, `list_commits`,
  `list_review_changes`, `read_review_patch`.
- `git/exec.rs`: spawns Git with explicit argument arrays, bounded output
  (4 MiB per stream), a 10 second deadline, and kill-on-drop cancellation.
  Repository-scoped runs share one hardened builder; no-index diffs get
  their own isolated stdin command.
- `git/validate.rs`: path, ref, scope, and flag-combination validation, plus
  repository classification through `git rev-parse --is-inside-work-tree`.
- `git/filters.rs`: detects configured clean and process filters and
  neutralizes them so review data never executes repository-defined
  filters.
- `git/parse.rs`: parses Git porcelain output (name-status, numstat,
  commits, worktrees, untracked paths) into typed domain structs.
- `review.rs`: assembles review data (changed files, patches, commits,
  refs) from Git results.
- `store.rs`: SQLite persistence (sqlx) for repositories, pins, and
  settings, plus path canonicalization. Migrations live in
  `src-tauri/migrations`.

## Git as the semantic authority

The native Git CLI is the only Git engine; there is no libgit2. The CLI
matches each user's installed Git exactly (repo formats, ref semantics,
diff behavior), streams output that can be bounded and cancelled, and keeps
Git crashes in a child process instead of the app. Queries use stable
porcelain formats, and invocations whose stderr is machine-parsed pin the
locale to C. The full rationale is in the repo `AGENTS.md`.

## Frontend (`src/`)

A flat React + Vite app: `App.tsx` (shell and review views), `settings.tsx`,
`diff.ts` (diff presentation helpers), and `navigation.ts` (back and forward
review history). Unit tests for the two utilities run with `npm run
test:unit`.

## Persistence

SQLite stores repositories, pin order, and settings. Review flows are
read-only and write nothing back to Git; see
[safety-model.md](safety-model.md).
