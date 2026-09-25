# Safety model

Review is read-only, and the runtime is built so that inspecting a
repository cannot mutate it or execute code it defines. Projects are
inspected where they live: a desktop-local project on the viewer's
machine, a desktop remote project on its own host through the user's own
`ssh`, and a server-backed project on the server's own disks. The store
holds a team's identities and review state, and both humans and agents
reach it over the same authenticated endpoint; the boundaries below hold
for every caller on every face.

## Review flows never mutate Git

- No staging, committing, checkout, pushing, or worktree management from
  any review path, present or planned, on any face (desktop IPC, the
  command API, or the agent face).
- Reviews are computed from read-only Git plumbing: diff, rev-parse,
  for-each-ref, log, ls-files, ls-tree, cat-file, merge-base, worktree
  list. The per-worktree change-count probe runs status with
  --no-optional-locks so it can never refresh or lock the index.
- The Git writes sit outside review computation, and all of them are
  bounded fetch paths. The project fetch runs `git fetch --all --prune`,
  which updates remote-tracking refs only. The review-content fetch
  re-fetches one named remote branch with the configured partial-clone
  filter suspended so that branch's blobs land on disk. Neither runs as a
  side effect of opening or reading a surface, and every review
  computation reads whatever state the last fetch left behind. A fetch is
  initiated by the local user's Fetch action, pinged by an authenticated
  agent through the endpoint's `refresh_repo` method, or run by an
  authenticated human through the command API's `fetch_project` /
  `fetch_review_objects` routes; the app owns the operation in every case
  and runs it on the same hardened path. For a desktop remote project the
  fetch runs one hop away on the project's own host, against the host's
  configured remotes, through the same allowlist choke point and with no
  refspec of any kind beyond `--all --prune`; the server tier's fetch
  paths run on the server's own disks. Local re-reads of fetched state
  are read-only and run on their own schedule (on window focus and a
  quiet interval); no fetch ever runs automatically.
- The review-content fetch is a named-branch recovery wherever it runs:
  on the desktop it re-fetches a branch of a local clone, and on the
  server it runs the same named-branch re-fetch on the server's own
  disks. Remote hosts keep their own clone state, so there is no
  remote-host equivalent.
- Agents can register a repository through the endpoint's `add_repo`
  method: the same validation and store write the UI's folder dialog
  runs, read-only over Git (it verifies the path is a work tree and
  stores the row). Humans register repositories through the command
  API's `open_repo` route, which is the same store path. Removing a
  repository is a member action on both human faces (`remove_repo`) and
  cascades the shared review state server-side; it is not exposed to
  agents. Repositories without a remote never spawn a fetch.

## Shared state, equal members, one admin

- The server's users are named accounts with per-user bearer tokens,
  created by an admin through the admin routes; there is no open
  registration. The first account is bootstrapped by the
  `worktreeview-server create-admin` subcommand on the host and is the
  first admin. Members are otherwise equal: everyone connected sees the
  same projects, reviews, threads, and events.
- Human management (list and create users, mint, list, and revoke user
  tokens) and agent-token management (list, create, delete) are admin-only
  routes, enforced server-side from the authenticated identity; no client
  claim is trusted. Deleting the last admin is refused by the server.
- Comments and threads are shared with one recorded ownership decision:
  any member may edit or delete any human-authored comment, and an agent
  may edit or delete only comments carrying its own token's ownership;
  agent-owned comments are never human-editable. Replies and resolve
  toggles are open to every actor.
- Every mutation attributes to its actor: human actions record the
  authenticated account's name in events and comment authorship exactly as
  agent actions record the token's identity. Deleted accounts and tokens
  stop authenticating immediately, while their attributed history stays
  behind.

## Spawn hygiene

- Git is spawned with explicit argument arrays and never shell
  interpolation, so metacharacters in paths, refs, or file names cannot
  become commands.
- Repository-defined code never runs during inspection. core.fsmonitor and
  diff.autoRefreshIndex are disabled per invocation, every diff passes
  `--no-ext-diff --no-textconv`, configured clean and process filters are
  neutralized, and no-index diffs run with an empty attributes file,
  `GIT_ATTR_NOSYSTEM`, outside the repository directory. The same
  neutralizations hold on a desktop remote project's host, where the
  diff's working directory is the host's temp directory, so no repository
  there can map its attributes onto the diff either; the server tier runs
  the same inspection on the server's own disks.
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
- A desktop remote project reads the same content from its own host.
  Working-changes and untracked content rides one fixed-argv read command,
  `cat` with only the shell-quoted host file path substituted, gated
  beside the git allowlist at the same choke point; the worktree root is
  re-verified on the host before the read, and untracked captures verify
  the file is untracked first. Untracked patches diff on the host with the
  same neutralized `git diff --no-index`, executed outside any repository
  directory there.
- Every content read shares the patch read's bounds: 16 MiB ceiling per
  stream, a 30 second deadline, kill-on-drop, and a binary check that
  refuses NUL-bearing content instead of rendering it. The command API's
  `read_review_file` and `read_review_file_bytes` routes run the same
  reads for authenticated humans on the server's own disks; raw bytes are
  returned as bytes, never rendered server-side.

## Opening reviewed files

- Handing a worktree file to the OS shell is a desktop-only action: it
  acts on the viewer's own machine, which is why the command API does not
  expose `open_review_file` or `open_log_dir` at all. A server cannot open
  files on a viewer's machine, and a viewer's client cannot open files on
  the server's.
- On the desktop, the action is user-initiated, never part of review
  computation, and spawns no Git and makes no network requests. The
  webview supplies only a diff-relative path; the command joins the
  worktree root, canonicalizes the result, and refuses anything that
  resolves outside that root, so renderer content cannot steer the OS
  opener to arbitrary locations. The action is offered whenever the review
  can name a plausible checkout root: the reviewed worktree, a branch
  target's owning worktree, or the repository's main worktree folder. The
  root is always an app-known worktree path, never renderer-supplied.
  Whether the file still exists on disk is only decided at click time,
  when the canonicalize step refuses a missing file with a visible error;
  no render-time filesystem probing runs, and the opened file may differ
  from the reviewed content when the checkout is not at the reviewed
  commit.
- A desktop remote project's reviewed file has no local path, so the open
  copies first: the file's content is fetched from the host under the
  reading-content bounds after the same worktree verification, written to
  a fresh nonce-named folder under the app's temp location, named after
  the reviewed file, and that local copy is what the OS opens or reveals.
  The renderer still supplies only the diff-relative path and never a
  resolved path; a failed fetch surfaces as a typed failure the frontend
  uses to hide the action for that project.
- A server-backed project has no checkout on the viewer's machine at
  all, so the desktop client offers no OS-open there; reviewed content
  is read through the command API under the bounds above.

## Network

- Review computations for a local project make no network requests;
  inspection commands spawn with `GIT_NO_LAZY_FETCH`, so reading a
  repository can never pull objects from a remote. A filtered partial
  clone (blobless or treeless) therefore reviews only what is already on
  disk: history renders, but a diff over unfetched content fails with an
  explicit `partial_clone_content` error instead of silently
  lazy-fetching.
- The outbound fetches are the bounded fetch paths described above,
  initiated by a local user, an authenticated human over the command
  API, or an agent ping; they contact only the repository's own
  configured remotes and never run as part of review computation. On the
  desktop tier the project fetch executes one hop away on a remote
  project's own host through the same choke point; the server tier's
  fetches run on the server's own disks. The review-content recovery
  stays a local-clone operation (desktop local projects, or the server's
  own disks) with no remote-host equivalent.
- A desktop remote project's review computations contact only that
  project's configured SSH host, reached with the user's own `ssh` binary
  and its config, agent, and known hosts. The same read-only plumbing
  runs on the host: every remote git invocation passes one allowlist at a
  single choke point, the read-only plumbing set plus the neutralized
  no-index diff and, outside git, the fixed-argv `cat` read template whose
  only variable slot is the shell-quoted file path. The identity of that
  allowlist check is the same choke point for validation and for batched
  review reads, and anything outside it is refused before a connection is
  used. The same neutralizations apply on the host (filters never
  execute, ext-diff and textconv are off, the locale is pinned where
  output is machine-parsed), so hosting the read elsewhere runs no
  repository-defined code there either.
- Remote reads carry no credentials of their own. ssh runs in batch mode
  and never answers a passphrase or host-key prompt; a rejected key or an
  unaccepted host key fails the read with an actionable message that
  points at the terminal-side fix. Where the platform supports it, ssh
  multiplexes the app's invocations over one connection (ControlMaster on
  non-Windows hosts); the app stores no key material and keeps no
  connection daemons alive beyond ssh's own persistence.
- Connection and authentication failures on remote projects surface as
  explicit offline or stale states on remote payloads, never as raw error
  surfaces, with the age of the last successful read attached. Host-side
  Git failures keep their own diagnostics. Statelessness makes
  reconnection free: a failed read retries nothing on its own; the next
  user- or refresh-driven load simply runs again.
- The endpoint is the app's one inbound network surface, served inside
  the app (or server) process. On the desktop it binds the address and
  port configured in Settings (loopback `127.0.0.1:9888` by default; a
  bind failure is shown in Settings and never blocks startup) and serves
  the raw JSON-RPC methods `post_review` and `refresh_repo` plus the
  stateless MCP face at `POST /mcp`; the server binds its configured
  address with a loopback default, and a failed bind is fatal because a
  headless server has nothing else to do. Bearer authentication is
  evaluated once per request and resolves a token to exactly one actor:
  an agent token or a human user token. Each face accepts one kind: the
  agent JSON-RPC/MCP face runs on agent tokens, the command API routes
  run on human tokens, and the SSE event stream accepts both. Tokens are
  per-agent and per-user rows; only SHA-256 hashes are stored, secrets
  are shown once at creation, and the desktop config file carries the
  current boot's default agent token. Beyond loopback the token is the
  real authentication boundary, now for humans as much as for agents;
  locally it still guards accidents.
- Transport limits apply to every face alike: bounded heads and bodies, a
  stall deadline per request, concurrent connection tasks, and identical
  refusals for missing, wrong, and revoked tokens so callers cannot probe
  which tokens exist.
- The desktop client is also an outbound network client of user-configured
  servers. Connecting a WorktreeView server in Settings stores its URL and
  the account's bearer token in the local store, plaintext, at the same
  trust level as the rest of the local store under the local threat model
  below. The webview then calls that server's command API directly with
  the token, reading reviews, comments, history, and project state, and
  triggering the same bounded fetches on the server's side; the CSP's
  `connect-src` is widened to `http:`/`https:` for exactly this purpose,
  while script sources stay unchanged. Every server call traces to
  explicit user configuration: saving the connection (one authenticated
  probe), adding a project by its host path, or opening that project's
  surfaces. There is no discovery, telemetry, or background syncing, and
  server event-stream (SSE) consumption rides the same seam. Pins and the
  activity seen cursor for server-backed projects stay client-local, so
  connecting to a server writes nothing on the server except what its
  own API already permits.
- There is no browser face: clients are the desktop app and API clients,
  every request carries its bearer token explicitly, and cookies,
  sessions, and CSRF have no surface to exist in. CORS exists only so the
  bundled webview's engine delivers its own token-bearing fetches:
  preflights are answered for the three Tauri webview origins on the
  faces the webview consumes (`/api/*`, `/events`) with the reflected
  origin (no wildcard), other callers get header-free responses, and the
  token stays the authentication boundary. Exposing the server beyond a
  private network is a deployment decision: terminate TLS at a reverse
  proxy in front of the loopback-bound server, and treat the proxy as the
  network boundary; the app itself never terminates TLS.
- The local threat model is unchanged: any process running as the user
  (or the server's user) can already read the store and the config file,
  so the token guards against stale clients and accidents, not against
  user-level processes.
- The server reviews repositories on its own disks. Remote-host
  inspection exists on the desktop tier through the user's own `ssh` as
  described above; a server-side equivalent (the server reaching other
  machines) remains future work.

## Test isolation

The desktop e2e suite runs the real bundled app in a container with no
network, no display sockets, a read-only fixture mount, a non-root user,
dropped capabilities, and CPU, memory, and PID limits. Cleanup removes only
the resources labeled with the run's own nonce. The remote-project spec
runs its throwaway sshd on loopback inside that container, and the
server-mode spec exercises the headless server and a connected client the
same way, so the suite still reaches no external network. See
[CONTRIBUTING.md](../CONTRIBUTING.md).
