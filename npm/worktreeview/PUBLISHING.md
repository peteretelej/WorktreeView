# Publishing the WorktreeView package

## Published

`worktreeview@0.0.1` was published on 2026-08-26. The npm registry currently
assigns both `prerelease` and `latest` to this version.

Keep `latest` in place. It gives normal unqualified npm installs a predictable
version, while the package README clearly states that the desktop application
is not distributed through npm yet. Do not unpublish v0.0.1; retaining the
published version secures the package name.

## Future releases

Run these commands from `npm/worktreeview/`, not the repository root.

```sh
npm whoami
npm pack --dry-run
npm publish --access public --tag prerelease
npm view worktreeview name version dist-tags --json
```

If `npm whoami` reports an authentication error, run `npm login` first. Keep the
explicit `--tag prerelease`; npm 10 does not reliably apply a dist-tag from
`publishConfig`, and this release should not become the default `latest` install.

Remove the `prerelease` publish tag when WorktreeView has a real npm
distribution that should become the default install.
