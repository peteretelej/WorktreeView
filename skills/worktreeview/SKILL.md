---
name: worktreeview
description: Set up and drive WorktreeView, a local desktop app where developers review Git worktrees and AI agents collaborate as first-class reviewers. Use when the user asks to install, start, or configure WorktreeView; to add a repository or project to it; to fetch or refresh projects; to read reviews, comments, or submissions; or to post, reply to, resolve, or edit review comments through its MCP endpoint.
---

# WorktreeView

WorktreeView is a local desktop app (Tauri) that gives developers a review
inbox over their Git worktrees. It runs a loopback API with two faces: an
MCP endpoint at `POST /mcp` (the one agents use) and a raw JSON-RPC face at
`POST /` for review submissions. Both need the same bearer token.

The app is human-owned. Agents read state, leave anchored comments, respond
in threads, and trigger fetches. Agents never remove repositories (the API
does not expose it), never run Git writes beyond the app's bounded fetch,
and never drive the UI - the human sees agent comments appear live in the
open review.

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
`127.0.0.1:9888`, configurable in Settings -> Agent API) and writes the
discovery file.

## 2. Connect the agent to the MCP endpoint

Read the discovery file for the current `{port, token}`:

- Windows: `%APPDATA%\com.etelej.worktreeview\agent-endpoint-dev.json`
  (dev builds) or `agent-endpoint.json` (installed builds)
- macOS: `~/Library/Application Support/com.etelej.worktreeview/`
- Linux: `$XDG_DATA_HOME/com.etelej.worktreeview/`

Two token options:

- **Zero-config**: use the discovery file's `token`. It is the built-in
  default token and renews at every app start and whenever the endpoint is
  restarted from Settings, so re-read the file whenever a request answers
  401.
- **Named token (stable)**: have the human mint one in Settings -> Agent
  API (name it after the agent). It survives restarts, gives the agent its
  own comment identity, and can be deleted independently. Prefer this for
  any lasting integration.

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
| `refresh_repo` | Run the app's bounded fetch for a repo, then it re-lists surfaces. |

Review identities are `{repo_path, base_sha, target_key, target_kind}`:
take `repo_path` from `list_repos`, the other fields from
`list_review_targets` (worktree reviews use the worktree path as
`target_key` with `target_kind` `worktree`; pass the path back exactly as
listed).

Typical flows:

- **Add a project**: `add_repo` with the absolute path. The app sidebar
  picks it up live; call `refresh_repo` if the human wants remote state
  fetched. Do not re-add in a loop; it is idempotent.
- **Review**: `list_review_targets` -> choose a target -> `list_comments`
  to read what exists -> post your findings with
  `create_comment` (line anchors need `side` and `start_line`; a range adds
  `end_line`). The human sees them live in the open review.
- **Respond**: read the thread with `list_comments`, `reply_comment` on the
  root, `resolve_thread` when the point is addressed. Only edit or delete
  comments your own token authored.
- **Update remote state**: `refresh_repo` after pushing or when the human
  asks for fresh remote-tracking refs.

## Rules and limits

- The comment body, anchor, severity, and `author_model` (max 200 chars)
  are validated by the same rules the human app enforces.
- Connections are served one request at a time with bounded body sizes
  (3 MiB); tolerate queuing and keep calls small.
- The token authenticates; `author_model` is self-reported display
  metadata. Do not spoof another agent's name via tokens - mint your own.
- Review flows never mutate Git state. `refresh_repo` runs the app's
  bounded fetch; that is the only Git write that exists.
- A 401 means the token is wrong, deleted, or rotated: re-read the
  discovery file (or ask the human for a fresh named token). A connection
  refusal means the app is not running - ask the human to start it.
