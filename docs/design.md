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
- Commit history is the default surface for an activated repository: a paged
  history view for the selected worktree's branch opens alongside the inbox
  and provides commit pickers. Dismissing it returns to the worktree table
  until the repository is activated again. Individual commits are reviewable
  against their parent, with the empty tree as the base for parentless
  commits.
- Reviewing or opening a worktree or branch remembers its identity and last
  resolved head. When the surface later disappears from the repository it
  stays listed flagged gone, and its history and committed-diff views still
  open from cached or re-derivable data; once Git can no longer resolve the
  recorded head, the view degrades to "content no longer available" instead
  of silently dropping the surface.
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
