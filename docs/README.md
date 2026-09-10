# WorktreeView docs

Durable documentation, one concern per page. Each page serves one reader
job: how-tos get a task done, references describe the system,
explanations give the why.

Start with [user-guide.md](user-guide.md) to use the app, or
[design.md](design.md) for the product model.

How-tos:

- [user-guide.md](user-guide.md): illustrated walkthrough of the app's
  features for humans
- [connect-an-agent.md](connect-an-agent.md): connect a coding agent:
  discovery, tokens, endpoints

Reference:

- [architecture.md](architecture.md): system layout: Tauri, React, Rust
  modules, IPC, SQLite
- [agent-submissions.md](agent-submissions.md): client contract for
  agent review submissions: schema, caps, rendering

Explanation:

- [design.md](design.md): product model: review-first worktree inbox,
  ref-based targets, the shared comment stream
- [safety-model.md](safety-model.md): read-only guarantees and Git
  spawn hygiene
- [performance.md](performance.md): large-repo and large-diff
  requirements
- [release.md](release.md): distribution channels and publishing plans

Process docs (setup, the dev loop, desktop testing) live in
[CONTRIBUTING.md](../CONTRIBUTING.md). The repo `AGENTS.md` routes
agents into these pages and states the update rule: a change that
alters behavior updates the affected page in the same change.
