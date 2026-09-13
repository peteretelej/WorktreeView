# Contributing

Rules for how this repo is built (architecture, safety, performance, docs)
live in [AGENTS.md](AGENTS.md). Product and engineering background lives in
[docs/](docs/README.md). This guide covers setup, the development loop, and
desktop testing.

## Prerequisites

- Node.js, selected with `nvm use` (toolchain pinned in the repo)
- Rust via rustup (toolchain pinned in `rust-toolchain.toml`)
- Git available on `PATH`

Platform specifics:

- Linux: install the
  [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/). On
  Debian/Ubuntu these include `libwebkit2gtk-4.1-dev`, `build-essential`,
  `curl`, `wget`, `file`, `libxdo-dev`, `libssl-dev`,
  `libayatana-appindicator3-dev`, and `librsvg-dev`.
- Windows: MSVC Rust toolchain and Visual Studio Build Tools with the
  Desktop development with C++ workload. WebView2 ships with current
  Windows releases, Git for Windows must be on `PATH`, and Docker Desktop
  with the WSL2 backend is needed for desktop e2e.

## Setup

```sh
nvm use
npm ci
```

`npm ci` gives a reproducible clean install; `npm install` is also valid
for normal local development.

## Development loop

```sh
npm run dev
```

This starts Vite, compiles the Rust backend, and opens the Tauri window;
it requires a display. When port 1420 is already taken (a second checkout,
another worktree's dev session), the launcher picks the next free port and
pins both Vite and the Tauri dev URL to it; set `WORKTREEVIEW_DEV_PORT` to
choose the starting port yourself.

For frontend-only work:

```sh
npm run dev:web
```

This serves the browser frontend at <http://localhost:1420/> without the
native backend. To serve behind a reverse proxy, set `WORKTREEVIEW_DEV_HOST`
to the hostname Vite should accept:

```sh
WORKTREEVIEW_DEV_HOST=worktreeview.example.com npm run dev:web
```

Port 1420 remains the default. On Linux, `npm run dev:lab` frees a stale
server and starts the frontend in one step. Inspect the local environment
with `npm run tauri info`.

To run a fully isolated instance (demo captures, parallel profiles), point
its data directory anywhere with `WORKTREEVIEW_DATA_DIR`:

```sh
WORKTREEVIEW_DATA_DIR=/some/dir npm run dev
```

The store, settings, tokens, and endpoint discovery file all live there,
so the instance shares nothing with the installed app or other dev
checkouts.

### Store migrations

Schema migrations in `src-tauri/migrations` are append-only: never edit or
delete a migration that has shipped; only add new numbered files. New
migration files need no registration: the e2e image copies the whole
`src-tauri/migrations/` directory (sqlx::migrate! embeds every `.sql` it
finds, so a build must never pin an explicit migration file list).
sqlx
records a checksum per applied migration, so an edited file makes every
existing store diverge on next launch, costing the user their saved repos,
pins, and settings: the store is set aside as `worktreeview.sqlite3.bak` and
rebuilt. Dev and installed builds share one data directory but keep separate
stores, and each dev checkout gets its own store file derived from its
target directory, so parallel worktrees with different schema versions never
trade migration skew through one file. Set `WORKTREEVIEW_DATA_DIR` to move
the data directory (store and endpoint registration) somewhere else.

Exception, recorded 2026-09-08: the review-comments feature consolidated
the first three migrations into a single `0001` baseline before any release
carried the three-file set forward; a pre-consolidation store diverges on
first launch and goes through the set-aside-and-rebuild path above, with
its data preserved in the backup. Future migrations must be append-only
again.

## Checks and builds

```sh
npm run check          # TypeScript checks
npm run test:unit      # unit tests for the navigation and diff utilities
npm run build:web      # frontend production build
npm run build          # native app and platform bundles
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

## Desktop testing

Headless desktop validation runs the real bundled app inside Docker, so the
host does not need Xvfb, WebKitGTK, DBus, Rust, or display configuration.
Linux hosts require Docker Engine with BuildKit/buildx support; Windows
hosts require Git Bash plus Docker Desktop with the WSL2 backend. Run the
supported test command with an absolute fixture path:

```sh
npm run test:desktop -- /absolute/path/to/git-repository
```

On Windows, invoke the command from Git Bash and pass the fixture as a
Windows path (`D:/path/to/git-repository`) or an MSYS path
(`/d/path/to/git-repository`); both forms are accepted.

The fixture must be a self-contained local Git directory whose Git metadata
is contained beneath that path. It is mounted read-only. Linked worktrees
are rejected because their Git metadata is outside the approved single
bind.

The command runs the real bundled Tauri app, UI, Rust IPC, native read-only
Git, SQLite reload and restart flows, failure handling, and bounded
collection checks inside Docker. It writes bounded diagnostics to one
`artifacts/tauri-e2e/<run-id>/` directory. The runtime is offline, non-root,
capability-dropped, and resource-bounded. Cleanup removes only the recorded
owned container and immutable loaded image, while retaining the shared
BuildKit cache. The complete inner deadline is 42 minutes 40 seconds; CI
allows 43 minutes for the command so runner-owned cleanup and evidence
finalization finish first.

Multiple checkouts can run the suite simultaneously. Runs claim no host
ports (the test container has no network), cleanup matches only the run's
own nonce labels, and artifacts are written to per-run directories. Each
run reserves 2 CPUs and 4 GB of memory inside the Docker VM, which is the
practical parallelism ceiling; `.wslconfig` is the knob when more
parallelism is needed on Windows.

The frontend preview is useful for presentation work, but it is not native
desktop validation; use `npm run dev` on a display-capable machine.

As a manual release check on a display-capable Linux machine, run the
normal desktop app and confirm that the real native GTK folder chooser
opens. Automated tests replace only the chooser result; the UI, backend,
Git, and persistence behavior after selection remains real.

For troubleshooting, check only that Docker (Engine on Linux, Docker
Desktop with the WSL2 backend on Windows) and BuildKit/buildx are
available, the fixture is an absolute self-contained Git path, and the
files in the reported run artifact directory. Do not use privileged mode,
host display sockets, writable fixture mounts, a networked test runtime,
or global Docker pruning.

## Releases

A release is a version-bump commit plus a tag. Run
`npm run set-version -- X.Y.Z` to sync the version across `package.json`,
`package-lock.json`, `src-tauri/tauri.conf.json`, and
`src-tauri/Cargo.toml`, commit, then tag. Between releases the manifests
on `main` keep the last released version, and development builds report
that version. Before tagging, make sure `main` is pushed and the desktop
e2e suite is green: pushing the tag publishes the release.

```sh
npm run set-version -- 0.2.2
git commit -am "bump version to 0.2.2"
git tag v0.2.2
git push origin main v0.2.2
```

The Release workflow re-derives the version from the pushed tag with the
same script and injects it into the manifests at build time, so a
forgotten bump commit cannot mislabel a release; it only leaves
development builds reporting the previous version.

The Release workflow builds Windows, macOS, and Linux bundles and
publishes them directly as the GitHub Release for the tag, with
generated release notes; publishing dispatches the desktop e2e suite.
Builds are unsigned.

## Documentation

Durable docs live in `docs/`, one concern per page, indexed in
[docs/README.md](docs/README.md). A change that alters behavior updates
the affected page in the same change. Keep the README minimal: pitch,
install, quick start.
