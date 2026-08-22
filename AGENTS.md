# GitCompare

## Product

GitCompare is an open-source, local-first, read-only Git worktree review inbox.
It helps developers review parallel human and agent work without mutating Git
state.

## Architecture

- Tauri 2 desktop shell
- React and Vite frontend
- Rust backend for process orchestration, normalization, SQLite, and typed IPC
- Native Git CLI as the semantic authority; do not add libgit2

Keep Git semantics and filesystem access out of the React layer. The webview
receives normalized domain data through narrow Tauri commands.

## Safety

- Never stage, commit, checkout, fetch, push, or manage worktrees in review
  flows.
- Spawn Git with explicit argument arrays, never shell interpolation.
- Do not run repository hooks, external diff drivers, text converters, or
  repository-defined code while inspecting repositories.
- The local review path makes no network requests.
- Do not implement remote/SSH behavior until its execution, trust, latency,
  freshness, reconnection, and persistence model is settled.

## Performance

Very large repositories and diffs are a hard requirement. Bound process output
and support cancellation in Rust. Use virtualized or windowed rendering for
large frontend collections and diffs; do not render a full large diff into the
DOM.

## Commands

```sh
bun install
bun run check
bun run build
cargo check --manifest-path src-tauri/Cargo.toml
bun run tauri dev
```
