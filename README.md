# WorktreeView

**Code review where you and your agents meet**

WorktreeView is an open-source desktop app that turns Git worktrees into a
shared review inbox for humans and coding agents. Open a repository and
review any worktree, branch, or commit against the right base without
checking it out. Your agents join as first-class reviewers over a local
API: they submit reviews, leave anchored comments, and reply in threads
while you watch it all land live.

![WorktreeView reviewing a commit with an agent submission in the comment stream](docs/images/screenshot.png)

## Features

- Review any worktree, branch, tag, or commit without checkout, computed
  against the right merge-base
- Working changes, all changes, and committed-only review presets per
  worktree
- A project overview with live change counts and searchable worktree and
  branch inventories
- Threaded markdown comments anchored to lines, files, or the whole
  review, with drift detection when code moves
- Agents submit reviews and findings into the same comment stream, read
  reviews over MCP, and double-check each other's work
- Read-only by design: reviews never modify Git state, and the refresh
  action's explicit fetch is the only network request

## Install

- **GitHub Releases**: grab the installer for your platform from the
  [latest release](https://github.com/peteretelej/WorktreeView/releases/latest).
  Builds are unsigned, so Windows SmartScreen and macOS Gatekeeper warn on
  first launch (macOS: right-click the app and choose Open)
- **Microsoft Store**: coming soon

## Use it yourself

Launch the app, add a repository, and click a worktree: the review opens
against the fork point with your uncommitted work included. Comment by
selecting lines in the diff, and copy any thread as markdown for your
notes or tools. The illustrated walkthrough lives in
[docs/user-guide.md](docs/user-guide.md).

## Connect your agent

The app serves a local MCP endpoint your coding agent can talk to the
moment it launches. The quickest route is the bundled agent skill, which
teaches your agent installation, connection, and the day-to-day review
workflow:

```sh
npx skills add peteretelej/WorktreeView
```

To wire a client by hand, point its MCP entry at
`http://127.0.0.1:<port>/mcp` with the bearer token from the discovery
file the app writes on startup. Setup steps and client examples are in
[docs/connect-an-agent.md](docs/connect-an-agent.md); the full API
contract for submissions and comments is
[docs/agent-submissions.md](docs/agent-submissions.md).

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
