# WorktreeView

## Product

WorktreeView is an open-source desktop app for reviewing code across local and
remote Git worktrees. It is built for developers running multiple coding agents
in parallel.

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
npm install
npm run check
npm run build:web
cargo check --manifest-path src-tauri/Cargo.toml
npm run dev
npm run build
```
