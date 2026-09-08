# Safety model

Review is read-only, and the runtime is built so that inspecting a
repository cannot mutate it or execute code it defines.

## Review flows never mutate Git

- No staging, committing, checkout, pushing, or worktree management from
  any review path, present or planned.
- Reviews are computed from read-only Git plumbing: diff, rev-parse,
  for-each-ref, log, ls-files, worktree list. The per-worktree change-count
  probe runs status with --no-optional-locks so it can never refresh or lock
  the index.
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

## Network

- Review computations make no network requests; inspection commands spawn
  with `GIT_NO_LAZY_FETCH`, so reading a repository can never pull objects
  from a remote.
- The fetch above is the single deliberate network operation: it contacts
  only the repository's own configured remotes, exactly as the user's Git
  would from a terminal, with a wider 60 second deadline for slow links.
  Remote and SSH review are still not implemented; they wait until their
  execution, trust, latency, freshness, reconnection, and persistence model
  is settled.

## Test isolation

The desktop e2e suite runs the real bundled app in a container with no
network, no display sockets, a read-only fixture mount, a non-root user,
dropped capabilities, and CPU, memory, and PID limits. Cleanup removes only
the resources labeled with the run's own nonce. See
[CONTRIBUTING.md](../CONTRIBUTING.md).
