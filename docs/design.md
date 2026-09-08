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

## Reviews as durable objects

- A review session persists its comments locally, keyed by review identity
  `(repo_path, base_sha, target_key, target_kind)`. The base and target are
  resolved SHAs (or the worktree path for a live worktree review), so a
  moved branch starts a fresh session by construction. Review scope and
  layout direction are not part of the identity: the same comments show in
  both scopes and either layout direction.
- Comments anchor to the review, to a file, or to a logical line range in a
  file (LEFT is the old side of the unreversed diff, RIGHT the new side).
  Every line comment captures a hash of the anchored lines' text plus a
  bounded snippet at write time, because that content is uncapturable later.
- Drift is detected against the loaded patch: unchanged content at the same
  line is current; the same content near the original line re-anchors the
  inline display and shows a moved marker; anything else shows an outdated
  badge in the comment stream alongside the write-time snippet. File-level
  and review-level comments do not drift.
- Threads are a root comment plus flat replies; resolve/reopen lives on the
  root. Authors are the local human user (`you`; no account system in v0)
  or agents, and the merged comment stream filters by author. Editing a
  comment rewrites its body in place; the delete action inside the edit
  composer permanently removes the comment, and deleting a root takes its
  replies. Comments are
  created by selecting a line or range in the diff, with review-level and
  file-level entry points always available.
- Anchoring stays a reading-first gesture: clicking code only selects, and
  text selection never disturbs the row anchors. The composer opens only
  from the floating Comment chip: clicking, shift-clicking, or dragging a
  line number arms it under the picked range, and selecting text arms it
  under the selection with the text pre-filled as a quoted excerpt, so part
  of a line comments like a line does. The chip overlays the diff and never
  shifts the rows beneath it.
- Comments are markdown end to end: the composer has a formatting toolbar,
  Ctrl+B/I/E/K shortcuts (bold, italics, code, link), a preview tab, and
  Ctrl+Enter submit; bodies render through the same sanitized renderer for
  humans and agents, including GFM tables, task lists, and strikethrough.
- Comments are copy-first for handoff to agents and other tools: every
  comment card copies that comment as markdown with author, severity, and
  anchor context; the stream header copies all visible threads as a
  structured markdown export; each agent submission card copies the whole
  submitted review.

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
