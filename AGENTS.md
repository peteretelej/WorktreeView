# WorktreeView

Review-first desktop app for code review across Git worktrees, built for
developers running multiple coding agents in parallel. Humans and agents
review together: the app serves a loopback MCP/JSON-RPC endpoint where
agents read reviews, submit findings, and collaborate in comment threads.

Stack: Tauri 2, React + Vite frontend, Rust backend (process
orchestration, normalization, SQLite, typed IPC). The native Git CLI is
the only Git engine; the rationale lives in
[docs/architecture.md](docs/architecture.md).

## Project structure

- `src/`: React + Vite frontend. `App.tsx` is the shell and review
  views; `diff.ts`, `highlight.ts`, `navigation.ts`, `reviewPresets.ts`,
  `surfaces.ts`, `comments.tsx`, `canvas.tsx`, and `markdown.tsx` carry
  the domain helpers. Git semantics and filesystem access stay out of
  this layer; the webview receives normalized domain data through
  narrow, typed Tauri commands.
- `src-tauri/src/`: Rust backend. `commands.rs` is the thin typed IPC
  surface; `git/` spawns and parses the CLI (`exec.rs`, `validate.rs`,
  `filters.rs`, `parse.rs`); `review.rs`, `reviews.rs`, `overview.rs`,
  `retrospection.rs`, `cache.rs`, `store.rs`, `agents.rs`, and
  `transport.rs` (the app's one inbound network surface) do the rest.
  The module map lives in [docs/architecture.md](docs/architecture.md).
- `src-tauri/migrations/`: append-only sqlx migrations.
- `e2e/`: headless desktop e2e suite, run in Docker.
- `docs/`: durable docs, one concern per page, indexed in
  [docs/README.md](docs/README.md).
- `skills/worktreeview/`: the installable agent skill that teaches app
  usage to coding agents.
- `npm/worktreeview/`: the name-squat npm package, not the app.

## Commands

```sh
npm install                     # or npm ci for a reproducible clean install
npm run check                   # TypeScript checks
npm run test:unit               # frontend utility tests
npm run build:web               # frontend production build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
npm run dev                     # full desktop dev run (needs a display)
npm run build                   # native app and platform bundles
npm run test:desktop -- /absolute/fixture/path   # Docker desktop e2e
```

`test:desktop` runs the real bundled app in Docker: Linux hosts need
Docker Engine with BuildKit, Windows hosts need Git Bash plus Docker
Desktop with the WSL2 backend. The fixture must be a standalone
self-contained local Git repository (a linked worktree cannot be one),
and evidence lands under `artifacts/tauri-e2e/<run-id>/`.

## Testing

- Frontend utility changes: `npm run test:unit`.
- Rust changes: `cargo test --manifest-path src-tauri/Cargo.toml`.
- Review-flow or IPC behavior changes: run the desktop e2e suite before
  handing over; it validates the real app end to end.
- A change that alters behavior updates the affected docs page in the
  same change.

## Code style

- Comments only for constraints the code cannot express; match the
  surrounding density.
- Keep presentation in the webview and IO in Rust; new capabilities
  enter as narrow typed commands, not broad data dumps.
- Spawn Git only through the shared hardened builder in `git/exec.rs`
  (explicit argument arrays, bounded output, deadlines, kill-on-drop);
  porcelain parsing belongs in `git/parse.rs`.

## Git workflow

- One concern per commit; titles short, lowercase, imperative, no
  conventional-commit prefixes.
- Feature work runs in a dedicated worktree when the current checkout is
  busy or the change outgrows one sitting.
- Never push to any remote unless explicitly asked.

## Boundaries

Always:

- Pin `LC_ALL`/`LANG` to C on invocations whose stderr is machine-parsed.
- Keep migrations append-only: add new numbered files, never edit or
  delete shipped ones; the full policy lives in CONTRIBUTING.md.
- Run `npm run check` and the relevant test suites before handing work
  over.

Ask first:

- New dependencies, frontend or Rust.
- Deviating from the performance model (rendering strategy, scroll
  machinery, output ceilings); the model and its accepted costs live in
  [docs/performance.md](docs/performance.md).
- New IPC commands or endpoint methods.

Never:

- Never stage, commit, checkout, push, or manage worktrees in review
  flows. The refresh action's explicit `git fetch` (remote-tracking refs
  only) is the one permitted Git write, and it never runs as part of
  review computation.
- Never add libgit2 or any in-process Git engine.
- Never run repository-defined code (hooks, external diff drivers, text
  converters, clean or process filters) while inspecting repositories.
- Never add network access to review paths: the agent endpoint is the
  one inbound surface and the refresh fetch the one outbound operation.
  No remote/SSH behavior until its model is settled; see
  [docs/safety-model.md](docs/safety-model.md).

## Docs

Durable docs live in `docs/`, one concern per page, indexed in
[docs/README.md](docs/README.md); do not mirror the index here. When
answering usage questions, route instead of re-deriving: using the app
to [docs/user-guide.md](docs/user-guide.md); connecting an agent to
[docs/connect-an-agent.md](docs/connect-an-agent.md); the API contract
to [docs/agent-submissions.md](docs/agent-submissions.md). Development
setup and the dev loop live in [CONTRIBUTING.md](CONTRIBUTING.md).
