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

## Webview side

- Large collections and diffs are rendered virtualized or windowed. A full
  large diff is never rendered into the DOM.

## Desktop e2e as a floor

The desktop e2e suite includes bounded collection checks against the real
app, so the requirement is validated, not just documented. Evidence for
each run lands under `artifacts/tauri-e2e/<run-id>/`.
