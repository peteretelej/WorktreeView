# Safety model

Review is read-only, and the runtime is built so that inspecting a
repository cannot mutate it or execute code it defines.

## Review flows never mutate Git

- No staging, committing, checkout, fetching, pushing, or worktree
  management from any review path, present or planned.
- Reviews are computed from read-only Git plumbing: diff, rev-parse,
  for-each-ref, log, ls-files, worktree list.

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

## No network

- The local review path makes no network requests.
- Remote and SSH review are not implemented; they wait until their
  execution, trust, latency, freshness, reconnection, and persistence model
  is settled.

## Test isolation

The desktop e2e suite runs the real bundled app in a container with no
network, no display sockets, a read-only fixture mount, a non-root user,
dropped capabilities, and CPU, memory, and PID limits. Cleanup removes only
the resources labeled with the run's own nonce. See
[CONTRIBUTING.md](../CONTRIBUTING.md).
