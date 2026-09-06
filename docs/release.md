# Release

WorktreeView is Apache-2.0 licensed. Re-licensing needs contributor
consent, so third-party code only enters under Apache-2.0 terms.

## Channels

- GitHub Releases: the primary distribution channel once the first version
  is tagged.
- Microsoft Store: planned. The README carries a placeholder to replace
  with the listing link at submission time.
- npm: the `worktreeview` package name and org are claimed. The published
  v0.0.1 exists to hold the name (prerelease tag; see
  `npm/worktreeview/PUBLISHING.md`) and is not the desktop app. Tauri
  applications cannot ship as native binaries through npm; if npm becomes a
  real channel it would follow the platform-binary wrapper pattern (an
  npm package that downloads per-platform builds on install), which is
  undecided.

## Documentation site

A dedicated github.io docs site is planned, published with
`peteretelej/starlight-action` (private at the time of writing). `docs/` is
the content source: plain markdown, one concern per page, so publishing
needs only frontmatter and navigation config.

## CI

GitHub Actions runs the desktop e2e suite (the same `npm run test:desktop`
command developers run locally) only on manual dispatch and published
releases, and retains failure diagnostics for 14 days.
