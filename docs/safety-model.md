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
- The one Git write sits outside review computation: the Fetch action
  runs `git fetch --all --prune`, which updates remote-tracking refs only.
  It never runs as a side effect of opening or reading a surface, and
  every review computation reads whatever state the last fetch left
  behind. The fetch is initiated by the user, or pinged by an
  authenticated agent through the endpoint's `refresh_repo` method; the
  app owns the operation in both cases and runs it on the same path.
  For a remote project the same fetch runs one hop away on the project's
  own host, against the host's configured remotes, through the same
  allowlist choke point and with no refspec of any kind beyond
  `--all --prune`; the one-Git-write invariant is unchanged.
  Local re-reads of that state are read-only and run on their own
  schedule (on window focus and a quiet interval); the fetch itself
  never runs automatically.
- Agents can also register a repository through the endpoint's `add_repo`
  method: the same validation and store write the UI's folder dialog runs,
  read-only over Git (it verifies the path is a work tree and stores the
  row). Removing a repository is not exposed to agents; that stays a human
  action in the UI.
  Repositories without a remote never spawn it.

## Spawn hygiene

- Git is spawned with explicit argument arrays and never shell
  interpolation, so metacharacters in paths, refs, or file names cannot
  become commands.
- Repository-defined code never runs during inspection. core.fsmonitor and
  diff.autoRefreshIndex are disabled per invocation, every diff passes
  `--no-ext-diff --no-textconv`, configured clean and process filters are
  neutralized, and no-index diffs run with an empty attributes file,
  `GIT_ATTR_NOSYSTEM`, outside the repository directory. The same no-index
  hygiene holds on a remote project's host, where the diff's working
  directory is the host's temp directory, so no repository there can map
  its attributes onto the diff either.
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
- A remote project reads the same content from its own host. Working-changes
  and untracked content rides one fixed-argv read command, `cat` with only
  the shell-quoted host file path substituted, gated beside the git
  allowlist at the same choke point; the worktree root is re-verified on
  the host before the read, and untracked captures verify the file is
  untracked first. Untracked patches diff on the host with the same
  neutralized `git diff --no-index`, executed outside any repository
  directory there.
- Every content read shares the patch read's bounds: 16 MiB ceiling per
  stream, a 30 second deadline, kill-on-drop, and a binary check that
  refuses NUL-bearing content instead of rendering it. Remote reads inherit
  exactly those bounds; the NUL check stays caller-side on the text path,
  so the raw-byte path can still serve images untouched.

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
- A remote project's reviewed file has no local path, so the open copies
  first: the file's content is fetched from the host under the
  reading-content bounds after the same worktree verification, written to
  a fresh nonce-named folder under the app's temp location, named after
  the reviewed file, and that local copy is what the OS opens or reveals.
  The renderer still supplies only the diff-relative path and never a
  resolved path; a failed fetch surfaces as a typed failure the frontend
  uses to hide the action for that project.

## Network

- Review computations for a local project make no network requests;
  inspection commands spawn with `GIT_NO_LAZY_FETCH`, so reading a
  repository can never pull objects from a remote. A filtered partial
  clone (blobless or treeless) therefore reviews only what is already on
  disk: history renders, but a diff over unfetched content fails with an
  explicit `partial_clone_content` error instead of silently
  lazy-fetching.
- A remote project's review computations contact only that project's
  configured SSH host, reached with the user's own `ssh` binary and its
  config, agent, and known hosts. The same read-only plumbing runs on the
  host: every remote git invocation passes one allowlist at a single
  choke point, the read-only plumbing set plus the neutralized no-index
  diff and, outside git, the fixed-argv `cat` read template whose only
  variable slot is the shell-quoted file path. The identity of that
  allowlist check is the same choke point for validation and for batched
  review reads, and anything outside it is refused before a connection is
  used. The same neutralizations apply on the host (filters never execute,
  ext-diff and textconv are off, the locale is pinned where output is
  machine-parsed), so hosting the read elsewhere runs no
  repository-defined code there either.
- Remote reads carry no credentials of their own. ssh runs in batch mode
  and never answers a passphrase or host-key prompt; a rejected key or an
  unaccepted host key fails the read with an actionable message that
  points at the terminal-side fix. Where the platform supports it, ssh
  multiplexes the app's invocations over one connection (ControlMaster on
  non-Windows hosts); the app stores no key material and keeps no
  connection daemons alive beyond ssh's own persistence.
- Connection and authentication failures surface as explicit offline or
  stale states on remote payloads, never as raw error surfaces, with the
  age of the last successful read attached. Host-side Git failures keep
  their own diagnostics. Statelessness makes reconnection free: a failed
  read retries nothing on its own; the next user- or refresh-driven load
  simply runs again.
- Two deliberate outbound network operations exist, both user-initiated
  (or agent-pinged through the endpoint) and never part of review
  computation. The project fetch contacts only the repository's own
  configured remotes and updates remote-tracking refs, exactly as the
  user's Git would from a terminal; the endpoint's `refresh_repo` method
  pings this same fetch, and for a remote project the same shared fetch
  path executes it on the project's own host. The review-content fetch,
  offered when a review fails on missing partial-clone content, re-fetches
  one named branch with the configured clone filter suspended so that
  branch's blobs land on disk; it never converts the whole clone. That
  recovery stays a local-project operation by design: it re-fetches a
  named branch of a local clone, and remote hosts keep their own clone
  state, so there is no remote equivalent. Both operations run through the
  same hardened spawn (explicit argv, bounded output, kill-on-drop, a wide
  deadline for slow links), and every review computation reads whatever
  state the last fetch left behind.
- The agent endpoint is the app's one inbound network surface, served
  inside the app process: it binds the address and port configured in
  Settings (loopback `127.0.0.1:9888` by default; a bind failure is shown
  in Settings and never blocks startup) and serves the raw JSON-RPC
  methods `post_review` and `refresh_repo` plus the stateless MCP face at
  `POST /mcp`, all over bearer-token authentication. Tokens are per-agent
  rows; the config file in the app home carries the current
  boot's default token. Beyond loopback the token is the real
  authentication boundary; locally it still guards accidents. Transport
  internals (HTTP semantics, head and body caps, the hyper h1 bypass)
  are described in [architecture.md](architecture.md); the protocol
  surface is specified in [agent-submissions.md](agent-submissions.md).
- The local threat model is unchanged: any process running as the user
  can already read the app's store and the config file, so the token
  guards against stale clients and accidents, not against user-level
  processes.

## Test isolation

The desktop e2e suite runs the real bundled app in a container with no
network, no display sockets, a read-only fixture mount, a non-root user,
dropped capabilities, and CPU, memory, and PID limits. Cleanup removes only
the resources labeled with the run's own nonce. The remote-project spec
runs its throwaway sshd on loopback inside that container, so the suite
still reaches no external network. See
[CONTRIBUTING.md](../CONTRIBUTING.md).
