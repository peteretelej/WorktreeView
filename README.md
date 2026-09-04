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
On Windows, use the MSVC Rust toolchain and install Visual Studio Build Tools
with the Desktop development with C++ workload. WebView2 is included with
current Windows releases, and Git for Windows must be available on `PATH`.

Select the pinned Node.js toolchain and bootstrap a fresh checkout. `npm ci` is
recommended for a reproducible clean install; `npm install` is also valid for
normal local development.

```sh
nvm use
npm ci
```

Start the native desktop app. This starts Vite, compiles the Rust backend, and
opens the Tauri window:

```sh
npm run dev
```

For browser-only frontend work, start Vite without the native backend:

```sh
npm run dev:web
```

Open <http://localhost:1420/> to reach the browser frontend. To serve behind a
reverse proxy (e.g. a tailnet route or custom domain), set
`WORKTREEVIEW_DEV_HOST` to the hostname Vite should accept:

```sh
WORKTREEVIEW_DEV_HOST=worktreeview.example.com npm run dev:web
```

Port 1420 is fixed. On Linux, free a stale server with `fuser -k 1420/tcp`, or
use `npm run dev:lab` to free the port and start the frontend in one step.
Port-freeing convenience for macOS and Windows is deferred.

Use the Tauri doctor to inspect the local environment:

```sh
npm run tauri info
```

Build the native application and platform bundles:

```sh
npm run build
```

The desktop development command requires a display. Frontend-only checks and
builds can run on a headless host with `npm run check` and `npm run build:web`.

### Desktop testing

Headless desktop validation runs the real bundled app inside Docker, so the
host does not need Xvfb, WebKitGTK, DBus, Rust, or display configuration.
Linux hosts require Docker Engine with BuildKit/buildx support; Windows hosts
require Git Bash plus Docker Desktop with the WSL2 backend. Run the supported
test command with an absolute fixture path:

```sh
npm run test:desktop -- /absolute/path/to/git-repository
```

On Windows, invoke the command from Git Bash and pass the fixture as a Windows
path (`D:/path/to/git-repository`) or an MSYS path
(`/d/path/to/git-repository`); both forms are accepted.

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

Multiple checkouts can run the suite simultaneously. Runs claim no host ports
(the test container has no network), cleanup matches only the run's own nonce
labels, and artifacts are written to per-run directories. Each run reserves 2
CPUs and 4 GB of memory inside the Docker VM, which is the practical
parallelism ceiling; `.wslconfig` is the knob when more parallelism is needed
on Windows.

The frontend preview remains useful for presentation work, but it is not native
desktop validation. Use `npm run dev` on a display-capable development
machine. GitHub Actions runs the same desktop test command only for manual
dispatches and published releases, and retains failure diagnostics for 14 days.

As a manual release check on a display-capable Linux machine, run the normal
desktop app and confirm that the real native GTK folder chooser opens. Automated
tests replace only the chooser result; the UI, backend, Git, and persistence
behavior after selection remains real.

For troubleshooting, check only that Docker (Engine on Linux, Docker Desktop
with the WSL2 backend on Windows) and BuildKit/buildx are available, the
fixture is an absolute self-contained Git path, and the files in the reported
run artifact directory. Do not use privileged mode, host display sockets,
writable fixture mounts, a networked test runtime, or global Docker pruning.

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

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
