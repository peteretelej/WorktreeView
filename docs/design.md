- Pinning extends to individual worktrees and branches the way it works for
  repositories: pinned surfaces sort first in their repo group, wherever they
  still exist or not. A pinned surface that later disappears keeps its row
  inline with a gone badge; every other disappeared surface collects in a
  searchable per-repo Archived section so months of deleted branches stay
  findable without cluttering the live sidebar.
- Settings (appearance, interface zoom, diff display, dark and light
  themes) persist locally.

## Non-goals

- Review flows never stage, commit, checkout, fetch, push, or manage
  worktrees. See [safety-model.md](safety-model.md).
- No libgit2. The native Git CLI is the semantic authority; see
  [architecture.md](architecture.md).
- No network on the local review path. Remote and SSH review wait until
  their execution, trust, latency, freshness, reconnection, and persistence
  model is settled.
- No repository-defined code (hooks, filters, text converters) runs while
  inspecting repositories.

## Direction

Planned work includes the Attention view as a cross-repo review queue, an
AI-assisted review layer that annotates review views, and packaging for
distribution ([release.md](release.md)).
