# WorktreeView

**Code review UI for Git worktrees**

An open-source desktop app for reviewing code across local and remote Git
worktrees. Built for developers running multiple coding agents in parallel.

WorktreeView uses Tauri 2, React, and the native Git CLI. The Rust backend will
own Git process orchestration, output parsing, persistence, and typed IPC. The
React webview owns presentation and interaction only.

## Development

Install the [Tauri Linux prerequisites](https://v2.tauri.app/start/prerequisites/)
before desktop development. On Debian/Ubuntu, these include
`libwebkit2gtk-4.1-dev`, `build-essential`, `curl`, `wget`, `file`,
`libxdo-dev`, `libssl-dev`, `libayatana-appindicator3-dev`, and `librsvg2-dev`.

Select the pinned Node.js toolchain and bootstrap a fresh checkout:

```sh
nvm use
npm ci
```

Start the frontend:

```sh
npm run dev
```

Open <http://localhost:1420/> to reach it. To serve behind a reverse proxy (e.g.
a tailnet route or custom domain), set `WORKTREEVIEW_DEV_HOST` to the hostname
Vite should accept:

```sh
WORKTREEVIEW_DEV_HOST=worktreeview.example.com npm run dev
```

Port 1420 is fixed. On Linux, free a stale server with `fuser -k 1420/tcp`, or
use `npm run dev:lab` to free the port and start the frontend in one step.
Port-freeing convenience for macOS and Windows is deferred.

Use the Tauri doctor to inspect the local environment:

```sh
npm run tauri info
```

The desktop shell requires a display and runs on a desktop machine, not the
headless lab host:

```sh
npm run check
npm run build
npm run tauri dev
```

The frontend build and Rust checks can run without launching a desktop window.

## Current State

The repository persists locally opened Git folders in SQLite and discovers their
worktrees live through the native Git CLI. Review state, diffs, and exports are
not implemented yet. Those capabilities must enter through explicit Tauri
commands without giving the webview shell access.

## Safety

- Review flows never stage, commit, switch, merge, or delete worktrees.
- Native Git remains the semantic authority; no libgit2.
- The local review path makes no network requests.
- Remote/SSH support is planned, but implementation waits until its trust and
  execution model is settled.
