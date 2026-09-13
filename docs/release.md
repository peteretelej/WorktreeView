# Release

WorktreeView is Apache-2.0 licensed. Re-licensing needs contributor
consent, so third-party code only enters under Apache-2.0 terms.

## Channels

- GitHub Releases: the primary distribution channel once the first version
  is tagged.
- Microsoft Store: packaged MSIX submissions through Partner Center
  (product `9PM1BN0JZDB3`, reserved as an "MSIX or PWA app", so the Store
  hosts and signs the package). `npm run package:store` builds the app and
  produces the unsigned upload MSIX. For a local test build, generate the
  dev certificate once (`winapp cert generate --manifest
  packaging/msix/Package.appxmanifest --output devcert.pfx`, then
  `winapp cert install devcert.pfx` from an elevated shell) and run
  `npm run package:store -- --cert devcert.pfx`. The manifest in
  `packaging/msix/` carries the identity Partner Center assigned and must
  match it verbatim. The README carries a placeholder to replace with the
  listing link at submission time.
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

GitHub Actions runs two workflows:

- Desktop e2e: the same `npm run test:desktop` command developers run
  locally, on manual dispatch and published releases, with failure
  diagnostics retained for 14 days.
- Release: a `v*` tag push builds Tauri bundles for Windows (NSIS and
  MSI), macOS (dmg, x64 and arm64), and Linux (deb, rpm, AppImage) and
  publishes them to a GitHub Release for the tag, then dispatches the
  desktop e2e suite (a release created by the workflow token does not
  itself trigger other workflows). The app version comes from the tag:
  the workflow runs the same `scripts/set-version.mjs` used for release
  prep (`npm run set-version`) to inject it into the manifests at build
  time, so a forgotten bump commit on `main` cannot mislabel a release;
  it only leaves development builds reporting the previous version.

Release builds are unsigned: Windows shows a
SmartScreen warning, and macOS requires right-click Open (or
`xattr -cr /Applications/WorktreeView.app`) on first launch. There is no
updater channel yet, so installs upgrade by downloading the next
release.
