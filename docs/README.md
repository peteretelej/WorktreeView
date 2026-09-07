# WorktreeView docs

Durable documentation, one concern per page. Start with
[design.md](design.md) for the product model and
[architecture.md](architecture.md) for how it is built.

| Doc | Purpose |
| --- | --- |
| [design.md](design.md) | Product model: review-first worktree inbox, ref-based targets |
| [architecture.md](architecture.md) | System layout: Tauri, React, Rust modules, IPC, SQLite |
| [safety-model.md](safety-model.md) | Read-only guarantees and Git spawn hygiene |
| [agent-submissions.md](agent-submissions.md) | Client contract for agent review submissions: schema, caps, rendering |
| [performance.md](performance.md) | Large-repo and large-diff requirements |
| [release.md](release.md) | Distribution channels and publishing plans |

Process docs (setup, the dev loop, desktop testing) live in
[CONTRIBUTING.md](../CONTRIBUTING.md). The repo `AGENTS.md` mirrors this
index and states the update rule: a change that alters behavior updates the
affected page in the same change.
