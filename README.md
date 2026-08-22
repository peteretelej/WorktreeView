# GitCompare

Open-source, local-first, non-mutating Git worktree review inbox.

GitCompare uses Tauri 2, React, and the native Git CLI. The Rust backend will
own Git process orchestration, output parsing, persistence, and typed IPC. The
React webview owns presentation and interaction only.

## Development

```sh
bun install
bun run check
bun run build
bun run tauri dev
```

Linux desktop development requires the Tauri WebKitGTK and librsvg system
prerequisites. The frontend build and Rust checks can run without launching a
desktop window.

## Current State

The repository contains the production shell and local fixture data needed to
validate the design handoff. Git inspection, SQLite persistence, review state,
and exports are not implemented yet. Those capabilities must enter through
explicit Tauri commands without giving the webview shell access.

## Safety

- The review flow must never mutate Git state.
- Native Git remains the semantic authority; no libgit2.
- The local review path makes no network requests.
- Remote/SSH support is out of scope until its trust and execution model is
  settled.
