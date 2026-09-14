# Design

WorktreeView is a review-first worktree inbox. Developers running several
coding agents in parallel end up with many worktrees, branches, and
reviews in flight at once; this app is where a human reads all of it, and
where agents deliver their findings. Everything is read-only: reviews
explain state, they never change it. The model has four parts: the inbox
(sidebar and project overview), reviews (targets, bases, presets,
reading context), history, and the shared comment stream that humans and
agents write into.

## The inbox

The sidebar lists projects, pinned first, then recent. Each project
group stays quiet: unpinned children show only the repository's current
checkout, so a repo with dozens of agent worktrees and hundreds of
branches keeps the group scannable. Everything else, all local and
remote branches plus months of disappeared surfaces in a searchable
Archived tab, lives on the project overview behind the group's "All
worktrees & branches" link.

Pinning extends to individual worktrees and branches the way it does for
repositories: pinned surfaces sort first in their group, and a pinned
surface that later disappears keeps its row inline with a gone badge.
Remote branch pins count as live while their remote-tracking ref exists,
and a gone worktree reads as its worktree folder name rather than its
branch, so a deleted worktree checked out on main does not masquerade as
the main branch.

## The project overview

Opening a project lands on its overview, not a bare history surface. The
header answers "what is this project": copyable chips for the project
path, origin remote (or local-only), and default branch. A filter row
with tabs (Worktrees, Branches, Remote, Archived) carrying per-tab
counts, plus a search input, scopes the inventory directly on the page.

The worktree tab (main worktree first) shows each worktree's branch,
path, last commit (subject, author, age), and status chips: the changed
chip opens the working-changes review, the ahead/behind chip opens the
committed changes against the branch's upstream or the default branch,
and clicking a row opens the default review. The branch tabs list local
and remote-tracking branches newest-first with the same sync and
last-commit columns; clicking one reviews it without checkout. Branches
without upstreams show ahead/behind against the default branch, computed
locally. Upstream tracking reflects the last fetch, which the refresh
action runs (`git fetch --all --prune`) before re-reading local state.
Commit history remains a permanent sidebar fixture.

The project actions menu (pinned to the overview header) offers pin,
copy path, and remove. Remove deletes the project from the app's
registry only: the repository, worktrees, and history on disk are never
touched, and the removal is confirmed before it runs.

## Reviews

Any ref is a review target: a worktree, a local or remote-tracking
branch, a tag, or an individual commit, none of which require a
checkout.

A review's default base is the branch's fork point, not a branch tip.
Worktree reviews merge-base the checked-out branch against the local
primary branch; a remote-tracking branch merge-bases against the
remote's own default (`origin/HEAD`, else its main/master/develop),
because the local primary checkout is often weeks stale on machines that
review remote branches without checking them out, and a stale tip
inflates the change list with everything that already landed.

### Presets

A worktree review offers three presets: Working changes pins the base to
the checked-out branch tip so only uncommitted content shows, All
changes reviews everything against the base with uncommitted content
included, and Committed only drops the uncommitted content. "Working
changes" follows the working-directory-changes convention other Git
clients use.

### Reading context

Reading context never leaves the review surface. Gaps between hunks grow
inline expand controls that pull the hidden lines around a change into
the diff, and a Diff/File toggle swaps the patch pane's body to the
whole file as it exists on the diff's new side, with the patch's added
lines highlighted. The file list, review header, and comment stream stay
put through both: toggling back restores the diff, page and scroll state
included. Expanded lines are reading context, not comment anchors:
existing comments still render there, but new anchors stay on patch
lines, where write-time content and drift matching are defined.

## History

Commit history is a permanent sidebar fixture rather than a surface you
have to be in: the commit list follows the active ref (the reviewed
worktree or branch, the history surface's start point, or the selected
worktree in the worktree list) and stays anchored when a commit review
opens. Picking a commit shows that commit's own diff against its parent.
Each row carries a corner control that re-bases the review on that
commit, so the last good parent visible in the list becomes the review
base in one click. Rows stay scannable with a short author name, an
ultra-compact relative age, and decorations stripped to their ref names.

## Comments and agent submissions

Humans and agents write into one merged comment stream. Agents submit
whole reviews over the local endpoint and their findings land as
ordinary comments with severity and authorship; agents also read
reviews, reply in threads, and resolve. Editing or deleting an agent
comment stays with its authoring token. Setup and the protocol contract
live in [connect-an-agent.md](connect-an-agent.md) and
[agent-submissions.md](agent-submissions.md).

### Identity

A review session persists its comments locally, keyed by review identity
`(repo_path, base_sha, target_key, target_kind)`. The base and target
are resolved SHAs (or the worktree path for a live worktree review), so
a moved branch starts a fresh session by construction. Review scope and
layout direction are not part of the identity: the same comments show in
both scopes and either layout direction.

### Anchors and drift

Comments anchor to the review, to a file, or to a logical line range in
a file (LEFT is the old side of the unreversed diff, RIGHT the new
side). Every line comment captures a hash of the anchored lines' text
plus a bounded snippet at write time, because that content is
uncapturable later.

Drift is detected against the loaded patch: unchanged content at the
same line is current; the same content near the original line re-anchors
the inline display and shows a moved marker; anything else shows an
outdated badge in the comment stream alongside the write-time snippet.
File-level and review-level comments do not drift.

### Writing comments

Anchoring stays a reading-first gesture: clicking code only selects, and
text selection never disturbs the row anchors. The composer opens only
from the floating Comment chip: clicking, shift-clicking, or dragging a
line number arms it under the picked range, and selecting text arms it
under the selection with the text pre-filled as a quoted excerpt, so
part of a line comments like a line does. The chip overlays the diff and
never shifts the rows beneath it. Review-level and file-level entry
points stay always available outside the diff.

Threads are a root comment plus flat replies; resolve/reopen lives on
the root. Authors are the local human user (`you`; no account system in
v0) or agents, and the merged stream filters by author. Editing a
comment rewrites its body in place; the delete action inside the edit
composer permanently removes the comment, and deleting a root takes its
replies.

Comments are markdown end to end: the composer has a formatting toolbar,
Ctrl+B/I/E/K shortcuts (bold, italics, code, link), a preview tab, and
Ctrl+Enter submit; bodies render through the same sanitized renderer for
humans and agents, including GFM tables, task lists, and strikethrough.

Comments are copy-first for handoff between tools: every comment card
copies that comment as markdown with author, severity, and anchor
context; the stream header copies all visible threads as a structured
markdown export; each agent submission card copies the whole submitted
review.

## Settings

Settings (appearance, interface zoom, diff display, dark and light
themes) persist locally.

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
- No repository-defined code (hooks, filters, text converters) runs
  while inspecting repositories.

## Direction

The portal direction has shipped as Pulse: a cross-project Inbox with
backend-owned attention categories (including request-less Recent
comments and per-row last-activity previews), Reviews and Threads
listings, a thread conversation surface, and an Activity feed over the
app's append-only event log with a seen watermark and Mark all seen.
Planned work includes an AI-assisted review layer that annotates review
views, and packaging for distribution ([release.md](release.md)).
