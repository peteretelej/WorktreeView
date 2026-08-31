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

### Linux desktop testing

Headless Linux desktop validation requires Docker Engine with BuildKit/buildx
support. The host does not need Xvfb, WebKitGTK, DBus, Rust, or display
configuration. Run the supported test command with an absolute fixture path:

```sh
npm run test:desktop -- /absolute/path/to/git-repository
```

The fixture must be a self-contained local Git directory whose Git metadata is
contained beneath that path. It is mounted read-only. Linked worktrees are
rejected because their Git metadata is outside the approved single bind.

The command runs the real bundled Tauri app, UI, Rust IPC, native read-only Git,
SQLite reload and restart flows, failure handling, and bounded collection checks
inside Docker. It writes bounded diagnostics to one
`artifacts/tauri-e2e/<run-id>/` directory. The runtime is offline, non-root,
capability-dropped, and resource-bounded. Cleanup removes only the recorded
owned container and immutable loaded image, while retaining the shared BuildKit
cache. The complete inner deadline is 42 minutes 40 seconds; CI allows 43 minutes
for the command so runner-owned cleanup and evidence finalization finish first.

The frontend preview remains useful for presentation work, but it is not native
desktop validation. Use `npm run tauri dev` on a display-capable development
machine. GitHub Actions runs the same desktop test command only for manual
dispatches and published releases, and retains failure diagnostics for 14 days.

As a manual release check on a display-capable Linux machine, run the normal
desktop app and confirm that the real native GTK folder chooser opens. Automated
tests replace only the chooser result; the UI, backend, Git, and persistence
behavior after selection remains real.

For troubleshooting, check only that Docker Engine and BuildKit/buildx are
available, the fixture is an absolute self-contained Git path, and the files in
the reported run artifact directory. Do not use privileged mode, host display
sockets, writable fixture mounts, a networked test runtime, or global Docker
pruning.

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
