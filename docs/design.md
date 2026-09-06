# Design

WorktreeView is a desktop app for reviewing code across local Git worktrees,
built for developers running multiple coding agents in parallel.

## The model

- A review inbox over the repo -> worktree hierarchy. The sidebar lists
  pinned and recent repositories, their worktrees, and their branches.
- Worktrees and refs are both first-class review targets. Clicking a
  worktree or a branch opens a review in one click; nothing is ever checked
  out.
- A worktree review shows all changes against the right base by default:
  the merge-base, including uncommitted and untracked work. A committed-only
  toggle narrows the scope. Non-worktree targets (branches, tags) are
  committed-only by definition.
- Individual commits are reviewable against their parent, with the empty
  tree as the base for parentless commits. A paged history view provides
  commit pickers.
- Settings (appearance, diff display, dark and light themes) persist
  locally.

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
