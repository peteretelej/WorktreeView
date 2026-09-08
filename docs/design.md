- Pinning extends to individual worktrees and branches the way it works for
  repositories: pinned surfaces sort first in their repo group, wherever they
  still exist or not. A pinned surface that later disappears keeps its row
  inline with a gone badge. Remote branch pins count as live while their
  remote-tracking ref exists, and a gone worktree reads as its worktree
  folder name rather than its branch, so a deleted worktree checked out on
  main does not masquerade as the main branch. The rest of the sidebar stays
  capped: unpinned
  children show only the most recently committed worktrees, so a repo with
  dozens of agent worktrees and hundreds of branches keeps the group
  scannable; everything else (all local and remote branches, and months of
  disappeared surfaces in a searchable Archived tab) lives on the project
  overview, reachable from the group's "All worktrees & branches" link.
- Opening a project lands on its overview, not a bare history surface. The
  header answers "what is this project": copyable chips for the project path,
  origin remote (or local-only), and default branch, above a stats strip
  (worktree count, total uncommitted changes, branches ahead of their base,
  branch counts). A filter row with tabs (Worktrees, Branches, Remote,
  Archived) plus a search input scopes the inventory directly on the page.
  The worktree tab (main worktree first) shows each worktree's branch, path,
  last commit (subject, author, age), and status chips: the changed chip
  opens the working-changes view, the ahead/behind chip opens the committed
  changes against the branch's upstream or the default branch, and clicking
  a row opens the default review. The branch tabs list local and
  remote-tracking branches newest-first with the same sync and last-commit
  columns; clicking one reviews it without checkout. Branches without
  upstreams show ahead/behind against the default branch, computed locally;
  upstream tracking reflects the last fetch, which the refresh action runs
  (`git fetch --all --prune`) before re-reading local state. Commit history
  remains a permanent sidebar fixture.
- The project actions menu (pinned to the overview header) offers pin, copy
  path, and remove. Remove deletes the project from the app's registry only:
  the repository, worktrees, and history on disk are never touched, and the
  removal is confirmed before it runs.
- A worktree review offers three presets: Working changes pins the base to
  the checked-out branch tip so only uncommitted content shows, All changes
  reviews everything against the base with uncommitted content included, and
  Committed only drops the uncommitted content. "Working changes" follows
  the working-directory-changes convention other Git clients use.
- Settings (appearance, interface zoom, diff display, dark and light
  themes) persist locally.
- Commit history is a permanent sidebar fixture rather than a surface you
  have to be in: the commit list follows the active ref (the reviewed
  worktree or branch, the history surface's start point, or the selected
  worktree in the worktree list) and stays anchored when a commit review
  opens. Picking a commit shows that commit's own diff against its parent.

## Non-goals

- Review flows never stage, commit, checkout, push, or manage worktrees.
  The one Git write outside review computation is the refresh action's
  explicit `git fetch`, which updates remote-tracking refs only; see
  [safety-model.md](safety-model.md).
- No libgit2. The native Git CLI is the semantic authority; see
  [architecture.md](architecture.md).
- No network beyond the refresh action's explicit fetch. Remote and SSH
  review wait until their execution, trust, latency, freshness,
  reconnection, and persistence model is settled.
- No repository-defined code (hooks, filters, text converters) runs while
  inspecting repositories.

## Direction

Planned work includes the Attention view as a cross-repo review queue, an
AI-assisted review layer that annotates review views, and packaging for
distribution ([release.md](release.md)).
