use crate::CommandError;
#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};

pub(crate) const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

// Test-only spawn counter: one increment per spawned git child, the
// assertion mechanism for cache tests. While a counting window is open,
// spawns from other threads pause at the gate, so exact spawn-delta
// assertions observe exactly the window's own children. Spawns inside the
// window bypass the gate via its thread flag.
#[cfg(test)]
static SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static SPAWN_GATE: Mutex<()> = Mutex::new(());
#[cfg(test)]
thread_local! {
    static SPAWN_COUNTING: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn reset_spawn_count() {
    SPAWN_COUNT.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn spawn_count() -> u64 {
    SPAWN_COUNT.load(Ordering::Relaxed)
}

#[cfg(test)]
fn count_spawn() {
    let counting = SPAWN_COUNTING.with(Cell::get);
    // Hold the gate across the increment so a spawn either completes before
    // a counting window opens or waits until one closes, never in between.
    let _guard = (!counting).then(|| {
        SPAWN_GATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    });
    SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
}

// Runs `future` inside an exclusive counting window and returns its output
// plus the number of git children it spawned.
#[cfg(test)]
pub(crate) async fn spawn_counted<F, T>(future: F) -> (u64, T)
where
    F: Future<Output = T>,
{
    let guard = SPAWN_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    SPAWN_COUNTING.with(|counting| counting.set(true));
    reset_spawn_count();
    let output = future.await;
    let count = spawn_count();
    SPAWN_COUNTING.with(|counting| counting.set(false));
    drop(guard);
    (count, output)
}

// Suppresses console windows when a GUI-parented git spawn would otherwise
// flash one (children inherit the hidden console).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

pub(crate) async fn read_bounded<R: AsyncRead + Unpin>(mut reader: R) -> Result<Vec<u8>, CommandError> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| CommandError::new("git_execution", "Git output could not be read."))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > MAX_OUTPUT {
            return Err(CommandError::new(
                "git_output_too_large",
                "Git output was too large.",
            ));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

async fn reap_after_kill(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

pub(crate) fn git_execution_error(stderr: &[u8]) -> CommandError {
    let message = if stderr.is_empty() {
        "Git command failed.".to_string()
    } else {
        String::from_utf8_lossy(stderr).into_owned()
    };
    CommandError::new("git_execution", message)
}

fn dir_git_command(
    path: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<Command, CommandError> {
    let path = path.to_str().ok_or_else(|| {
        CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
    })?;
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.autoRefreshIndex=false",
        ])
        .env("GIT_NO_LAZY_FETCH", "1");
    for (name, value) in env {
        command.env(name, value);
    }
    if args.starts_with(&["-c", "core.quotePath=false"]) {
        command
            .args(["-c", "core.quotePath=false", "-C", path])
            .args(&args[2..]);
    } else {
        command.args(["-C", path]).args(args);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console(&mut command);
    Ok(command)
}

pub(crate) async fn run_git_with_env(
    path: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<(i32, Vec<u8>, Vec<u8>), CommandError> {
    let mut child = dir_git_command(path, args, env)?
        .spawn()
        .map_err(|_| CommandError::new("git_execution", "Git could not be started."))?;
    #[cfg(test)]
    count_spawn();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stdout was unavailable."))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stderr was unavailable."))?;
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));
    let result = timeout(GIT_TIMEOUT, async {
        let status = child
            .wait()
            .await
            .map_err(|_| CommandError::new("git_execution", "Git did not finish."))?;
        let stdout = stdout_task
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stdout failed."))??;
        let stderr = stderr_task
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stderr failed."))??;
        Ok::<_, CommandError>((status, stdout, stderr))
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok((status.code().unwrap_or(-1), stdout, stderr)),
        Ok(Err(error)) => {
            reap_after_kill(&mut child).await;
            Err(error)
        }
        Err(_) => {
            reap_after_kill(&mut child).await;
            Err(CommandError::new(
                "git_timeout",
                "Git did not respond within 10 seconds.",
            ))
        }
    }
}

pub(crate) async fn run_git(path: &Path, args: &[&str]) -> Result<(i32, Vec<u8>, Vec<u8>), CommandError> {
    run_git_with_env(path, args, &[]).await
}

pub(crate) fn stdin_git_command(args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "-c", "core.attributesFile="])
        .args(args)
        .env_remove("GIT_DIR")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env_remove("GIT_ATTR_SOURCE")
        // Spawn outside the launcher's directory: Git discovers repositories from
        // the current directory, which would let that repository's .gitattributes
        // and path prefix leak into these no-repository diffs.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console(&mut command);
    command
}

pub(crate) async fn run_git_with_stdin(
    mut command: Command,
    input: Vec<u8>,
) -> Result<(i32, Vec<u8>, Vec<u8>), CommandError> {
    let mut child = command
        .spawn()
        .map_err(|_| CommandError::new("git_execution", "Git could not be started."))?;
    #[cfg(test)]
    count_spawn();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stdin was unavailable."))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stdout was unavailable."))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stderr was unavailable."))?;
    let mut stdin_task = tokio::spawn(async move {
        stdin
            .write_all(&input)
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stdin could not be written."))?;
        stdin
            .shutdown()
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stdin could not be closed."))
    });
    let mut stdout_task = tokio::spawn(read_bounded(stdout));
    let mut stderr_task = tokio::spawn(read_bounded(stderr));
    let result = timeout(GIT_TIMEOUT, async {
        let status = child
            .wait()
            .await
            .map_err(|_| CommandError::new("git_execution", "Git did not finish."))?;
        (&mut stdin_task)
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stdin failed."))??;
        let stdout = (&mut stdout_task)
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stdout failed."))??;
        let stderr = (&mut stderr_task)
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stderr failed."))??;
        Ok::<_, CommandError>((status, stdout, stderr))
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok((status.code().unwrap_or(-1), stdout, stderr)),
        Ok(Err(error)) => {
            stdin_task.abort();
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdin_task.await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            reap_after_kill(&mut child).await;
            Err(error)
        }
        Err(_) => {
            stdin_task.abort();
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdin_task.await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            reap_after_kill(&mut child).await;
            Err(CommandError::new(
                "git_timeout",
                "Git did not respond within 10 seconds.",
            ))
        }
    }
}

pub(crate) fn acceptable_diff_exit(exit_code: i32, no_index: bool) -> bool {
    exit_code == 0 || (no_index && exit_code == 1)
}

pub(crate) fn git_args<'a>(args: &'a [String]) -> Vec<&'a str> {
    args.iter().map(String::as_str).collect()
}

pub(crate) fn repo_stdin_git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    #[cfg(unix)]
    use crate::testutil::{test_git, test_pool, test_repo};
    #[cfg(unix)]
    use crate::review::review_changes;
    #[cfg(unix)]
    use crate::read_review_patch;
    #[cfg(unix)]
    use std::process::Command as StdCommand;

    #[test]
    fn accepts_only_no_index_difference_exit_code() {
        assert!(acceptable_diff_exit(0, false));
        assert!(acceptable_diff_exit(1, true));
        assert!(!acceptable_diff_exit(1, false));
        assert!(!acceptable_diff_exit(2, true));
    }

    #[test]
    fn dir_commands_apply_only_requested_extra_environment() {
        let dir = std::env::temp_dir();
        let probe_args = ["rev-parse", "--is-inside-work-tree"];
        let pinned_command =
            dir_git_command(&dir, &probe_args, &[("LC_ALL", "C"), ("LANG", "C")]).unwrap();
        let pinned = pinned_command.as_std().get_envs().collect::<Vec<_>>();
        assert_eq!(
            pinned
                .iter()
                .find(|(name, _)| *name == OsStr::new("LC_ALL"))
                .and_then(|(_, value)| *value),
            Some(OsStr::new("C"))
        );
        assert_eq!(
            pinned
                .iter()
                .find(|(name, _)| *name == OsStr::new("LANG"))
                .and_then(|(_, value)| *value),
            Some(OsStr::new("C"))
        );
        let plain_command = dir_git_command(&dir, &probe_args, &[]).unwrap();
        let plain = plain_command.as_std().get_envs().collect::<Vec<_>>();
        assert!(plain
            .iter()
            .all(|(name, _)| *name != OsStr::new("LC_ALL") && *name != OsStr::new("LANG")));
    }

    #[test]
    fn stdin_diff_commands_isolate_attribute_environment() {
        let numstat = [
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "-z",
            "--",
            "/dev/null",
            "-",
        ];
        let patch = [
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "-U3",
            "--",
            "/dev/null",
            "-",
        ];
        for args in [&numstat[..], &patch[..]] {
            let command = stdin_git_command(args);
            let environments = command.as_std().get_envs().collect::<Vec<_>>();
            assert_eq!(
                environments
                    .iter()
                    .find(|(name, _)| *name == OsStr::new("GIT_ATTR_NOSYSTEM"))
                    .and_then(|(_, value)| *value),
                Some(OsStr::new("1"))
            );
            assert!(environments.iter().any(|(name, value)| {
                *name == OsStr::new("GIT_ATTR_SOURCE") && value.is_none()
            }));
            assert!(environments
                .iter()
                .any(|(name, value)| *name == OsStr::new("GIT_DIR") && value.is_none()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn review_commands_disable_repository_helpers() {
        use std::os::unix::fs::PermissionsExt;

        let repo = test_repo("repository-helpers");
        let marker = repo.join("helper-ran");
        let helper = repo.join("helper.sh");
        std::fs::write(&helper, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        test_git(
            &repo,
            &["config", "core.fsmonitor", helper.to_str().unwrap()],
        );
        test_git(
            &repo,
            &["config", "diff.external", helper.to_str().unwrap()],
        );
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();

        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--short"])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let _ = StdCommand::new("git")
            .args(["-c", "core.fsmonitor=false", "-C"])
            .arg(&repo)
            .args(["diff", "--", "tracked.txt"])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let pool = test_pool().await;
        let index = review_changes(
            &pool,
            repo.to_str().unwrap(),
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert!(index.files.iter().any(|file| file.path == "tracked.txt"));
        assert!(!marker.exists());

        let patch = read_review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("changed"));
        assert!(!marker.exists());

        std::fs::remove_dir_all(repo).unwrap();
    }
}
