# WorktreeView

**Code review UI for Git worktrees**

An open-source desktop app for reviewing code across local Git worktrees.
Built for developers running multiple coding agents in parallel: open a
repository, click a worktree, branch, or commit, and see what changed
against the right base, without checking anything out.

![WorktreeView reviewing a commit in the git-cache-retrospection worktree](docs/images/screenshot.png)

## Features

- Repository tree with pinned and recent repos plus each project's latest
  worktrees
- Project overview with live per-worktree change counts, searchable
  worktree, branch, remote, and archived inventories, and one-click
  working-changes reviews
- Worktree reviews against the merge-base, including uncommitted and
  untracked work by default, with a committed-only toggle
- Review any branch, tag, or individual commit without checkout
- Paged commit history with one-click commit reviews
- Appearance and diff display settings with dark and light themes
- Read-only reviews: they never modify Git state; the refresh action's
  explicit fetch is the only network request

## Install

- GitHub Releases: grab the installer for your platform from the
  [latest release](https://github.com/peteretelej/WorktreeView/releases/latest).
  Builds are unsigned, so Windows SmartScreen and macOS Gatekeeper
  will warn on first launch
- Microsoft Store: coming soon <!-- TODO: replace with the Store listing link once submitted -->
- npm: the `worktreeview` name is reserved for a future distribution
  channel; the npm package is not the desktop app

## Development

```sh
nvm use
npm ci
npm run dev
```

Fast checks for a change: `npm run check` (TypeScript) and
`cargo test --manifest-path src-tauri/Cargo.toml` (backend). Headless
desktop e2e against a fixture repository:

```sh
npm run test:desktop -- /absolute/path/to/git-repository
```

Setup, the full development loop, and the desktop testing guide live in
[CONTRIBUTING.md](CONTRIBUTING.md). Design and architecture live in
[docs/](docs/README.md).

## License

[Apache 2](LICENSE)
