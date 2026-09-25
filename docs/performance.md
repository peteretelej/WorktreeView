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

## Remote projects

- Network round trips are the remote cost driver, so read groups batch:
  a load action composes its git commands into one ssh invocation and
  demultiplexes the framed output locally. A committed review load runs
  in a single round trip; worktree statuses run two (enumeration, then
  the per-worktree probes); a branch inventory runs at most two
  (unconditional reads, then the merged and divergence probes that need
  the resolved default branch). The callers own the grouping exactly as
  they own their local spawn sequences.
- Where the platform supports it, ssh multiplexes invocations over one
  connection (ControlMaster on non-Windows hosts), so batched groups
  after the first skip connection setup. Windows OpenSSH has no
  multiplexing; batching already bounds the round trips, so Windows runs
  one connection per group.
- Every remote invocation carries a 60 second deadline sized to include
  connection setup, with kill-on-drop, and the 16 MiB output ceiling
  applies to each batch. Failed loads are explicit offline or stale
  states; nothing retries in the background, so a flaky link costs
  nothing while the app sits idle.

## Webview side

- The changed-files pane renders the whole list at once: there is no
  pager, so the directory tree always spans the full changeset. The tree,
  its per-directory counts, and filtering derive client-side from the
  existing `{path, status}` list, and rows are one-line buttons, so the
  DOM scales with the changeset's file count at a fraction of the cost of
  diff rows (the same trade the row stream below makes).
- Both patch and full-file views render one continuous row stream in
  native flow (`src/stream.ts` builds the rows; there is no custom scroll
  code). The whole stream is present in the DOM and the browser owns
  scrolling, so the scrollbar and the content can never desync. The DOM
  scales with the diff - memory, not responsiveness, is the trade-off.
  Files past the render cap (50,000 rows) decline with open-externally
  actions instead of freezing, and the e2e suite validates a 6000-line
  patch end to end.
- Selected file patches are cached in memory for the life of the review,
  keyed by the same identity the backend fetch uses. Revisiting a file
  renders from the cache with no Git spawn; the cache resets when the
  review's base, target, scope, or direction changes, matching the index's
  snapshot freshness.
- Context expansion and the full-file view fetch the file's content once
  per file through a bounded read (the same 16 MiB ceiling) and keep a
  smaller in-memory cache than patches. Expansion splices fetched gap
  lines into the existing hunk model, so both views flow through the same
  continuous stream: a fully expanded hunk or a large file just grows the
  stream the user scrolls through.
- Diff highlighting never delays first paint: lines render as plain text
  immediately, and token spans swap in once the worker responds. Patch
  hunks tokenize whole, in order, on load; the full-file view tokenizes in
  bounded chunks that land progressively. Tokenization runs
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
