# Performance

Very large repositories and very large diffs are a hard requirement, not a
later optimization target.

## Rust side

- Git output is bounded: every captured stream enforces a 4 MiB ceiling and
  fails closed past it.
- Every Git invocation carries a 10 second deadline; expiry kills the child
  process rather than leaking work.
- No in-process Git engine: large operations stay in a child process, so
  their memory growth and crashes never land in the app.
- Commit history is cached in SQLite keyed by resolved SHAs: a warm
  history open skips the `git log` spawn, and deep-page and
  ancestry-probe costs are paid once per SHA key instead of on every
  open. `git log` still runs, bounded, whenever the key changes.
- A patch read resolves only symbolic ref endpoints: literal SHA
  endpoints are content-addressed and validated by the diff itself.

## Webview side

- Large collections and diffs are rendered virtualized or windowed. A full
  large diff is never rendered into the DOM.
- Selected file patches are cached in memory for the life of the review,
  keyed by the same identity the backend fetch uses. Revisiting a file
  renders from the cache with no Git spawn; the cache resets when the
  review's base, target, scope, or direction changes, matching the index's
  snapshot freshness.

## Desktop e2e as a floor

The desktop e2e suite includes bounded collection checks against the real
app, so the requirement is validated, not just documented. Evidence for
each run lands under `artifacts/tauri-e2e/<run-id>/`.
