//! The app home: one known directory holding everything the app persists
//! (config.json with the agent endpoint payload, the SQLite store, and the
//! published agent skill). Profile-root placement keeps the path identical
//! on every OS and outside AppData virtualization for packaged (MSIX)
//! builds, so external processes such as agents always find it at the
//! documented location.

use std::path::{Path, PathBuf};

pub(crate) const HOME_ENV_VAR: &str = "WORKTREEVIEW_DATA_DIR";
pub(crate) const HOME_ARG: &str = "--home";

/// The agent skill ships inside the binary and is published into the home,
/// so an installed app carries its own version-matched copy and never
/// depends on the repository. The list must mirror `skills/worktreeview/`
/// exactly; the drift test in this module fails when the folder and the
/// bundle disagree.
pub(crate) const SKILL_BUNDLE: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../../skills/worktreeview/SKILL.md")),
    (
        "performing-review.md",
        include_str!("../../skills/worktreeview/performing-review.md"),
    ),
    (
        "requesting-review.md",
        include_str!("../../skills/worktreeview/requesting-review.md"),
    ),
];

/// Where the published skill copy lives, relative to the app home; mirrors
/// the repository layout so the two stay mentally interchangeable.
pub(crate) fn skill_dir(home: &Path) -> PathBuf {
    home.join("skills").join("worktreeview")
}

/// Publish the bundled skill into the home, rewriting a file only when its
/// content drifted so backup tools and readers see stable mtimes. Files in
/// the folder that the bundle does not name are left alone.
pub(crate) fn publish_skill_bundle(home: &Path) -> std::io::Result<()> {
    let dir = skill_dir(home);
    std::fs::create_dir_all(&dir)?;
    for (name, contents) in SKILL_BUNDLE {
        let path = dir.join(name);
        let drifted = std::fs::read_to_string(&path)
            .map(|existing| existing != *contents)
            .unwrap_or(true);
        if drifted {
            std::fs::write(&path, contents)?;
        }
    }
    Ok(())
}

/// Default home under the user profile; debug builds live beside it so a
/// dev checkout never claims an installed release's data or endpoint.
pub(crate) fn default_home_dir(debug: bool) -> &'static str {
    if debug {
        ".worktreeview-dev"
    } else {
        ".worktreeview"
    }
}

/// The `--home <path>` launch override, from either the spaced or the
/// `--home=<path>` form. Later flags lose to the first; non-argument
/// tokens and other flags are skipped.
pub(crate) fn home_arg_from<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Ok(arg) = arg.into_string() else {
            continue;
        };
        if let Some(value) = arg.strip_prefix(&format!("{HOME_ARG}=")) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        } else if arg == HOME_ARG {
            // A following flag is never the value: `--home --other` means
            // --home was forgotten its value, not that the home is a flag.
            let value = match args.next() {
                Some(value) => value,
                None => continue,
            };
            let Ok(value) = value.into_string() else {
                continue;
            };
            if !value.trim().is_empty() && !value.starts_with('-') {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Launch override wins over the environment, which wins over the default
/// under the user profile. Empty values count as unset; relative paths are
/// kept as given and resolve against the process working directory.
pub(crate) fn resolve_home(
    arg: Option<&str>,
    env: Option<&str>,
    home_root: &Path,
    debug: bool,
) -> PathBuf {
    let chosen = arg
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| env.map(str::trim).filter(|value| !value.is_empty()));
    match chosen {
        Some(value) => PathBuf::from(value),
        None => home_root.join(default_home_dir(debug)),
    }
}

/// One-time store migration from the legacy per-OS app data directory into
/// the home. Copies only when the home has no store yet, so a deliberate
/// store reset never resurrects legacy data, and always leaves the legacy
/// directory untouched as the backup. Best effort: a failed copy means a
/// fresh home while the legacy data stays where it was.
pub(crate) fn migrate_legacy_store(legacy_dir: &Path, home: &Path, db_name: &str) {
    if legacy_dir == home {
        return;
    }
    let target = home.join(db_name);
    if target.exists() {
        return;
    }
    let source = legacy_dir.join(db_name);
    if !source.exists() {
        return;
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if !from.exists() {
            continue;
        }
        let to = PathBuf::from(format!("{}{suffix}", target.display()));
        if std::fs::copy(&from, &to).is_err() {
            // A half-migrated set (a store without the WAL that holds its
            // latest commits) must not stay behind: it would block every
            // later launch from retrying the migration. Drop all three so
            // the next start copies the whole set again.
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", target.display())));
            let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", target.display())));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        default_home_dir, home_arg_from, migrate_legacy_store, publish_skill_bundle,
        resolve_home, skill_dir, HOME_ARG, SKILL_BUNDLE,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn default_home_dir_separates_channels() {
        assert_eq!(default_home_dir(false), ".worktreeview");
        assert_eq!(default_home_dir(true), ".worktreeview-dev");
    }

    #[test]
    fn resolve_prefers_arg_over_env_over_default() {
        let root = Path::new("/Users/dev");
        assert_eq!(
            resolve_home(Some("/tmp/a"), Some("/tmp/b"), root, false),
            PathBuf::from("/tmp/a")
        );
        assert_eq!(
            resolve_home(None, Some("/tmp/b"), root, false),
            PathBuf::from("/tmp/b")
        );
        assert_eq!(
            resolve_home(None, None, root, false),
            root.join(".worktreeview")
        );
        assert_eq!(
            resolve_home(None, None, root, true),
            root.join(".worktreeview-dev")
        );
    }

    #[test]
    fn empty_overrides_count_as_unset() {
        let root = Path::new("/Users/dev");
        assert_eq!(
            resolve_home(Some("   "), Some("  "), root, false),
            root.join(".worktreeview")
        );
        assert_eq!(resolve_home(Some(""), Some("/tmp/b"), root, false), PathBuf::from("/tmp/b"));
    }

    #[test]
    fn home_arg_parses_both_forms() {
        assert_eq!(home_arg_from(args(&[HOME_ARG, "/tmp/x"])), Some("/tmp/x".into()));
        assert_eq!(home_arg_from(args(&["--home=/tmp/y"])), Some("/tmp/y".into()));
        assert_eq!(home_arg_from(args(&["--home", "  /tmp/z  "])), Some("/tmp/z".into()));
    }

    #[test]
    fn home_arg_ignores_missing_or_blank_values() {
        assert_eq!(home_arg_from(args(&[HOME_ARG])), None);
        assert_eq!(home_arg_from(args(&[HOME_ARG, "--other"])), None);
        assert_eq!(home_arg_from(args(&["--home="])), None);
        assert_eq!(home_arg_from(args(&["--home", "   "])), None);
        assert_eq!(home_arg_from(args(&["--port=1"])), None);
    }

    #[test]
    fn migration_copies_store_and_sidecars() {
        let legacy = crate::testutil::test_path("home-migrate-legacy");
        let home = crate::testutil::test_path("home-migrate-home");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(legacy.join("worktreeview.sqlite3"), b"db").unwrap();
        std::fs::write(legacy.join("worktreeview.sqlite3-wal"), b"wal").unwrap();

        migrate_legacy_store(&legacy, &home, "worktreeview.sqlite3");

        assert_eq!(std::fs::read(home.join("worktreeview.sqlite3")).unwrap(), b"db");
        assert_eq!(std::fs::read(home.join("worktreeview.sqlite3-wal")).unwrap(), b"wal");
        assert!(legacy.join("worktreeview.sqlite3").exists(), "legacy stays as backup");
    }

    #[test]
    fn migration_never_overwrites_a_live_store() {
        let legacy = crate::testutil::test_path("home-migrate-keep-legacy");
        let home = crate::testutil::test_path("home-migrate-keep-home");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(legacy.join("worktreeview.sqlite3"), b"old").unwrap();
        std::fs::write(home.join("worktreeview.sqlite3"), b"new").unwrap();

        migrate_legacy_store(&legacy, &home, "worktreeview.sqlite3");

        assert_eq!(std::fs::read(home.join("worktreeview.sqlite3")).unwrap(), b"new");
    }

    #[test]
    fn migration_skips_missing_legacy_or_same_directory() {
        let home = crate::testutil::test_path("home-migrate-skip-home");
        std::fs::create_dir_all(&home).unwrap();
        migrate_legacy_store(&home.join("missing"), &home, "worktreeview.sqlite3");
        assert!(!home.join("worktreeview.sqlite3").exists());

        std::fs::write(home.join("worktreeview.sqlite3"), b"db").unwrap();
        migrate_legacy_store(&home, &home, "worktreeview.sqlite3");
        assert_eq!(std::fs::read(home.join("worktreeview.sqlite3")).unwrap(), b"db");
    }

    #[test]
    fn failed_sidecar_copy_rolls_back_the_whole_set() {
        let legacy = crate::testutil::test_path("home-migrate-fail-legacy");
        let home = crate::testutil::test_path("home-migrate-fail-home");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(legacy.join("worktreeview.sqlite3"), b"db").unwrap();
        std::fs::write(legacy.join("worktreeview.sqlite3-wal"), b"wal").unwrap();
        // A directory sitting where the -shm sidecar must be read from makes
        // that copy fail after the main store already landed.
        std::fs::create_dir(legacy.join("worktreeview.sqlite3-shm")).unwrap();

        migrate_legacy_store(&legacy, &home, "worktreeview.sqlite3");

        assert!(!home.join("worktreeview.sqlite3").exists(), "no half-migrated store may stay behind");
        assert!(!home.join("worktreeview.sqlite3-wal").exists());
        assert!(!home.join("worktreeview.sqlite3-shm").exists());
    }

    #[test]
    fn skill_bundle_mirrors_the_repository_folder() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../skills/worktreeview");
        let mut expected: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let contents = std::fs::read_to_string(entry.path()).unwrap();
                (name, contents)
            })
            .collect();
        expected.sort();
        let mut embedded: Vec<(String, String)> = SKILL_BUNDLE
            .iter()
            .map(|(name, contents)| (name.to_string(), contents.to_string()))
            .collect();
        embedded.sort();
        assert_eq!(embedded, expected, "SKILL_BUNDLE must mirror skills/worktreeview exactly; update the bundle when the skill gains or loses files");
    }

    #[test]
    fn publishing_writes_the_bundle_and_leaves_unknown_files() {
        let home = crate::testutil::test_path("home-skill-publish");
        std::fs::create_dir_all(&home).unwrap();

        publish_skill_bundle(&home).unwrap();

        let dir = skill_dir(&home);
        for (name, contents) in SKILL_BUNDLE {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), *contents);
        }

        std::fs::write(dir.join("stray.txt"), "kept").unwrap();
        std::fs::write(dir.join("SKILL.md"), "stale").unwrap();
        publish_skill_bundle(&home).unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("stray.txt")).unwrap(), "kept");
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            SKILL_BUNDLE[0].1,
            "drifted bundled files are rewritten"
        );
    }
}
