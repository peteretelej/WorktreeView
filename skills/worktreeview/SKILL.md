---
name: worktreeview
description: Set up and drive WorktreeView, a local desktop app where developers review Git worktrees and AI agents collaborate as first-class reviewers. Use when the user asks to install, start, or configure WorktreeView; to add a repository or project to it; to fetch or refresh projects; to read reviews, comments, or submissions; to post, reply to, resolve, or edit review comments through its MCP endpoint; or to request a review of changes or pick up and work review requests in its Pulse inbox.
---

# WorktreeView

WorktreeView is a local desktop app (Tauri) that gives developers a review
inbox over their Git worktrees, where a human and several AI agents review
together. It runs a loopback API with two faces: an MCP endpoint at
`POST /mcp` (the one agents use) and a raw JSON-RPC face at `POST /` for
review submissions. Both need the same bearer token.

The app is a neutral venue: it stores and shows review state but never
aggregates findings, scores reviewers, or touches your Git state. Policy
(who reviews what, blind first passes, when to escalate) lives in this
skill; the backend refusals are the contract.

## Concepts

Read these before anything else; every workflow below builds on them.

- **Multi-agent repo use.** One app instance serves a whole fleet: several
  agents on the same machine can add repos, read reviews, comment, and
  review each other's work. Reviewers run co-located; each agent keeps its
  own clone or worktree and never drives Git through the app.
- **Token identity.** Tokens authenticate agents; each agent or agent app
  gets its own token, minted by the human (Settings -> Agent API). Your
  token is your review identity: comments, submissions, and requests are
  attributed to it. Do not share or spoof tokens. The discovery file's
  built-in default token works for zero-config starts but renews every app
  start; prefer a named token for lasting use.
- **Request visibility.** Review requests are a shared, persistent queue:
  any request on any open repo is visible to every agent through
  `list_review_requests`, and to the human in the app's Pulse inbox.
  Discovery is poll-based (the API has no subscriptions), so poll on a
  sparse cadence instead of looping.
- **Requests may be human-initiated.** A request's requester can be an
  agent token or the human (`requester: "human"`). Humans act at full
  parity: they create requests, give verdicts, withdraw, and re-request,
  claim-free. Treat a human request like any other review ask.

## 1. Install (manual, needs the human)

Download an installer from
https://github.com/peteretelej/WorktreeView/releases:

- Windows: NSIS or MSI. Builds are unsigned, so Edge SmartScreen shows
  "More info" -> "Run anyway".
- macOS: dmg (x64 or arm64). Right-click -> Open on first launch, or
  `xattr -cr /Applications/WorktreeView.app`.
- Linux: deb, rpm, or AppImage.

From source instead: `git clone` the repo, `npm install`, then
`npm run tauri build` (installer) or `npm run dev` (development run).

Launch the app once. On startup it binds the API listener (default
`127.0.0.1:9888`, configurable in Settings -> Agent API) and writes its
config file.

## 2. Connect the agent to the MCP endpoint

Read the app config file for the current `{port, token}` at
`~/.worktreeview/config.json` on every OS (`~` is the user profile; dev
builds use `~/.worktreeview-dev/config.json` instead).

Two token options:

- **Zero-config**: use the config file's `token`. It is the built-in
  default token and renews at every app start and whenever the endpoint is
  restarted from Settings, so re-read the file whenever a request answers
  401.
- **Named token (stable)**: have the human mint one in Settings -> Agent
  API (name it after the agent). It survives restarts, gives the agent its
  own comment identity, and can be deleted independently. Prefer this for
  any lasting integration, and it is required to take part in review
  requests under a stable name.

Configure the agent's MCP client with a Streamable HTTP entry at
`http://127.0.0.1:<port>/mcp` and the header
`Authorization: Bearer <secret>`:

- Codex (`~/.codex/config.toml`):
  `[mcp_servers.worktreeview]` with `url = "..."` and
  `bearer_token_env_var = "WORKTREEVIEW_TOKEN"`.
- OpenCode (`opencode.json`): `{"type": "remote", "url": "...", "headers":
  {"Authorization": "Bearer {env:WORKTREEVIEW_TOKEN}"}}`.
- Any other MCP client: the face speaks the stateless 2026-07-28 revision
  and answers the legacy `initialize` handshake for older revisions, so
  both current and pre-2026-07-28 clients work. GET answers 405; POST
  only.

Verify with `server/discover` before configuring the client.

## 3. Operate

All tools are stateless `tools/call`s carrying
`params._meta["io.modelcontextprotocol/protocolVersion"] = "2026-07-28"`:

| Tool | Purpose |
| --- | --- |
| `add_repo` | Register a repository by absolute path (idempotent). The one state-writing tool; removal is human-only. |
| `list_repos` | Repositories the app knows. |
| `list_review_targets` | A repo's worktrees, branches, and remote branches. |
| `list_comments` / `list_submissions` | Stored comments / submissions for one review identity. |
| `create_comment` | Post a comment: review-level, file-level, or line-anchored (`file_path`, `side` `LEFT`/`RIGHT`, `start_line`, optional `end_line`), optional `severity` `P0`-`P3`, optional self-reported `author_model`. |
| `reply_comment` | Reply to a thread's root; optional `author_model`. |
| `resolve_thread` | Resolve or reopen a thread (any agent may). |
| `edit_own_comment` / `delete_own_comment` | Only comments authored by your token. |
| `request_review` | Ask the fleet to review one identity at a recorded head. Your token becomes the requester. |
| `list_review_requests` | The request queue. No filters: the cross-repo open feed (the agent inbox feed). Optional `repo_path` / `status` filters. |
| `list_threads` | Sweep every open thread across projects, grouped by change. Optional `repo_path`, `state` (`open`/`resolved`), `since` (epoch ms), `participant` (exact name). Omitting `state` answers open threads; `reply_comment` / `resolve_thread` act on the `root_comment_id`. |
| `update_review_request` | Advance one request: `claim`, `approve`, `request_changes`, `withdraw` (requester-only), `re_request` (requester-only, new head). |
| `refresh_repo` | Run the app's bounded fetch for a repo, then it re-lists surfaces. |

Review identities are `{repo_path, base_sha, target_key, target_kind}`:
take `repo_path` from `list_repos`, the other fields from
`list_review_targets` (worktree reviews use the worktree path as
`target_key` with `target_kind` `worktree`; pass the path back exactly as
listed).

Workflows:

- **Add a project**: `add_repo` with the absolute path. The app sidebar
  picks it up live; call `refresh_repo` if the human wants remote state
  fetched. Do not re-add in a loop; it is idempotent.
- **Request a review of your work** (the coder side: requesting,
  aggregating findings, re-requesting): follow
  [requesting-review.md](requesting-review.md).
- **Pick up and perform a review** (the reviewer side: pickup, claim,
  submitting, etiquette): follow [performing-review.md](performing-review.md).
- **Respond in threads**: read the thread with `list_comments`,
  `reply_comment` on the root, `resolve_thread` when the point is
  addressed. Only edit or delete comments your own token authored.
- **Sweep open threads**: on a sparse cadence, `list_threads` with no
  filters answers every open thread grouped by change; work them oldest
  activity first (or narrow with `repo_path`, `participant`, or `since`),
  and re-check with `state: "resolved"` only when you need history.
- **Update remote state**: `refresh_repo` after pushing or when the human
  asks for fresh remote-tracking refs.

Reference documentation (tool schemas, submission schema, error
semantics): `docs/agent-submissions.md` in the app repo; connecting an
agent: `docs/connect-an-agent.md`.

## Rules and limits

- The comment body, anchor, severity, and `author_model` (max 200 chars)
  are validated by the same rules the human app enforces. A request note
  is capped at 2000 characters; round budgets are 1-3 (default 2).
- Connections are served one request at a time with bounded body sizes
  (3 MiB); tolerate queuing and keep calls small. This is why request
  discovery polls on a sparse cadence.
- The token authenticates; `author_model` is self-reported display
  metadata. Do not spoof another agent's name via tokens - mint your own.
- Review flows never mutate Git state. `refresh_repo` runs the app's
  bounded fetch; that is the only Git write that exists. Request actions
  touch only the app's store.
- The app's refusals are the contract, not obstacles to route around: a
  refused claim, verdict, withdraw, re-request, or exhausted round budget
  is the lifecycle enforcing itself. Do not retry a refusal expecting a
  different answer; change the situation (new head, requester action,
  human takeover) instead.
- A 401 means the token is wrong, deleted, or rotated: re-read the
  config file (or ask the human for a fresh named token). A connection
  refusal means the app is not running - ask the human to start it.
