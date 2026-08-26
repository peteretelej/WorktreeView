# WorktreeView

**Code review UI for Git worktrees**

An open-source desktop app for reviewing code across local and remote Git
worktrees. Built for developers running multiple coding agents in parallel.

WorktreeView uses Tauri 2, React, and the native Git CLI. The Rust backend will
own Git process orchestration, output parsing, persistence, and typed IPC. The
React webview owns presentation and interaction only.

## Development

```sh
npm install
npm run check
npm run build
npm run tauri dev
```

Install the [Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/)
for your platform before desktop development. The frontend build and Rust
checks can run without launching a desktop window.

## Current State

The repository contains the production shell and local fixture data needed to
validate the design handoff. Git inspection, SQLite persistence, review state,
and exports are not implemented yet. Those capabilities must enter through
explicit Tauri commands without giving the webview shell access.

## Safety

- Review flows never stage, commit, switch, merge, or delete worktrees.
- Native Git remains the semantic authority; no libgit2.
- The local review path makes no network requests.
- Remote/SSH support is planned, but implementation waits until its trust and
  execution model is settled.
