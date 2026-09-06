use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn test_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    pool
}

pub(crate) fn test_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "worktreeview-{label}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(crate) fn test_git(path: &Path, args: &[&str]) {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn test_repo(label: &str) -> PathBuf {
    let path = test_path(label);
    std::fs::create_dir(&path).unwrap();
    test_git(&path, &["init", "--quiet"]);
    test_git(&path, &["config", "user.name", "WorktreeView Tests"]);
    test_git(&path, &["config", "user.email", "tests@example.com"]);
    std::fs::write(path.join("tracked.txt"), "original\n").unwrap();
    test_git(&path, &["add", "tracked.txt"]);
    test_git(&path, &["commit", "--quiet", "-m", "initial"]);
    path
}

pub(crate) fn test_rev_parse(path: &Path, revision: &str) -> String {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", revision])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}
