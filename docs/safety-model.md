# Safety model

Review is read-only, and the runtime is built so that inspecting a
repository cannot mutate it or execute code it defines.

## Review flows never mutate Git

- No staging, committing, checkout, pushing, or worktree management from
  any review path, present or planned.
- Reviews are computed from read-only Git plumbing: diff, rev-parse,
  for-each-ref, log, ls-files, ls-tree, cat-file, merge-base, worktree
  list. The per-worktree change-count probe runs status with
  --no-optional-locks so it can never refresh or lock the index.
- The one Git write sits outside review computation: the refresh action
  runs `git fetch --all --prune`, which updates remote-tracking refs only.
  It is always user-initiated, never runs as a side effect of opening or
  reading a surface, and every review computation reads whatever state the
  last fetch left behind. Repositories without a remote never spawn it.

## Spawn hygiene

- Git is spawned with explicit argument arrays and never shell
  interpolation, so metacharacters in paths, refs, or file names cannot
  become commands.
- Repository-defined code never runs during inspection. core.fsmonitor and
  diff.autoRefreshIndex are disabled per invocation, every diff passes
  `--no-ext-diff --no-textconv`, configured clean and process filters are
  neutralized, and no-index diffs run with an empty attributes file,
  `GIT_ATTR_NOSYSTEM`, outside the repository directory.
- Invocations whose stderr is machine-parsed pin `LC_ALL` and `LANG` to C,
  so classification never depends on the user's Git language. All other
  invocations keep the user's locale for human-readable errors.

## Reading file content

- Context expansion and the full-file view fetch the reviewed file's
  content on the displayed diff's new side. Committed and reversed sides
  read through `ls-tree` plus `cat-file blob`, with the blob SHA taken from
  ls-tree's output so no renderer-assembled `rev:path` name reaches Git;
  working-changes reads go through the same cap-std path as untracked
  captures: relative paths only, the root pinned to a verified worktree,
  regular files only.
- Every content read shares the patch read's bounds: 16 MiB ceiling per
  stream, a 30 second deadline, kill-on-drop, and a binary check that
  refuses NUL-bearing content instead of rendering it.

## Opening reviewed files

- The patch pane can hand a worktree file to the OS default application or
  reveal it in the file manager. The action is user-initiated, never part of
  review computation, and spawns no Git and makes no network requests.
- The webview supplies only a diff-relative path; the command joins the
  worktree root, canonicalizes the result, and refuses anything that
  resolves outside that root, so renderer content cannot steer the OS
  opener to arbitrary locations.
- The action is offered whenever the review can name a plausible checkout
  root: the reviewed worktree, a branch target's owning worktree, or the
  repository's main worktree folder. The root is always an app-known
  worktree path, never renderer-supplied. Whether the file still exists on
  disk is only decided at click time, when the canonicalize step refuses a
  missing file with a visible error; no render-time filesystem probing
  runs, and the opened file may differ from the reviewed content when the
  checkout is not at the reviewed commit.

## Network

- Review computations make no network requests; inspection commands spawn
  with `GIT_NO_LAZY_FETCH`, so reading a repository can never pull objects
  from a remote.
- The refresh fetch is the single deliberate outbound network operation: it
  contacts only the repository's own configured remotes, exactly as the
  user's Git would from a terminal, with a wider 60 second deadline for
  slow links.
- The app accepts agent submissions on a loopback-only, token-gated
  endpoint inside the app process: it binds 127.0.0.1 only,
  authenticates with a per-boot bearer token published to a discovery
  file in the app data directory, and serves one write-only method
  (`post_review`). The endpoint serves axum HTTP semantics over a raw
  tokio connection loop; request heads are parsed with httparse, hyper's
  own parser, under bounded head, header, and body caps. hyper's h1
  connection layer is bypassed because it does not deliver responses on
  the current Windows host (upstream-report candidate). See
  [agent-submissions.md](agent-submissions.md).
- The local threat model is unchanged: any process running as the user
  can already read the app's store, so the token guards against stale
  clients and accidents, not against user-level processes.
- Remote and SSH review are still not implemented; they wait until their
  execution, trust, latency, freshness, reconnection, and persistence model
  is settled.

## Test isolation

The desktop e2e suite runs the real bundled app in a container with no
network, no display sockets, a read-only fixture mount, a non-root user,
dropped capabilities, and CPU, memory, and PID limits. Cleanup removes only
the resources labeled with the run's own nonce. See
[CONTRIBUTING.md](../CONTRIBUTING.md).
