# Performance

Very large repositories and very large diffs are a hard requirement, not a
later optimization target.

## Rust side

- Git output is bounded: every captured stream enforces a 16 MiB ceiling
  and fails closed past it.
- Every Git invocation carries a 30 second deadline; expiry kills the child
  process rather than leaking work. Large repositories (thousands of remote
  branches, cold caches) legitimately need tens of seconds on some hosts.
- No in-process Git engine: large operations stay in a child process, so
  their memory growth and crashes never land in the app.
- Commit history is cached in SQLite keyed by resolved SHAs: a warm
  history open skips the `git log` spawn, and deep-page and
  ancestry-probe costs are paid once per SHA key instead of on every
  open. `git log` still runs, bounded, whenever the key changes.
- A patch read resolves only symbolic ref endpoints: literal SHA
  endpoints are content-addressed and validated by the diff itself, and
  the untracked-file check inspects only the selected path instead of
  walking the whole worktree.

## Webview side

- Large collections and diffs are rendered virtualized or windowed. A full
  large diff is never rendered into the DOM.
- Both patch and full-file views render through one windowed row stream
  (`src/stream.ts`): a pure height model holds a fixed row height per row
  plus measured overrides (wrapped lines, comment cards), and only the
  rows around the scroll viewport mount, padded by a fixed pixel overscan.
  Fixed-height rows are exact from the first frame; measured rows correct
  the model after paint while the viewport stays anchored. There is no
  pager: scrolling moves the window instead of changing pages.
- Selected file patches are cached in memory for the life of the review,
  keyed by the same identity the backend fetch uses. Revisiting a file
  renders from the cache with no Git spawn; the cache resets when the
  review's base, target, scope, or direction changes, matching the index's
  snapshot freshness.
- Context expansion and the full-file view fetch the file's content once
  per file through a bounded read (the same 16 MiB ceiling) and keep a
  smaller in-memory cache than patches. Expansion splices fetched gap
  lines into the existing hunk model, so both views flow through the same
  windowed stream: a fully expanded hunk or a large file just grows the
  row run the window slides over, and the DOM never holds more than the
  mounted window.
- Diff highlighting never delays first paint: lines render as plain text
  immediately, and token spans swap in once the worker responds. Patch
  hunks tokenize whole as they scroll into view; the full-file view
  tokenizes in bounded chunks that land progressively. Tokenization runs
  in a Web Worker, so grammar CPU can never block rendering or input;
  per-line length and time budgets degrade pathological lines to plain
  text instead of hanging, superseded requests are discarded on file
  switch, and the oniguruma engine plus grammars load lazily inside the
  worker per language. Token results cache per content key in a bounded
  cache on the main thread, and files without a supported language skip
  highlighting entirely.

## Desktop e2e as a floor

The desktop e2e suite includes bounded collection checks against the real
app, so the requirement is validated, not just documented. Evidence for
each run lands under `artifacts/tauri-e2e/<run-id>/`.
