use super::exec::{git_execution_error, repo_stdin_git_command, run_git, run_git_with_stdin};
use crate::CommandError;
use std::path::Path;

pub(crate) fn unsupported_filter_error() -> CommandError {
    CommandError::new(
        "git_filter_unsupported",
        "This review cannot run because Git conversion filters apply to files in this review.",
    )
}

pub(crate) async fn configured_filter_names(path: &Path) -> Result<Vec<String>, CommandError> {
    let (exit_code, stdout, stderr) = run_git(
        path,
        &["config", "--get-regexp", r"^filter\..*\.(clean|process)$"],
    )
    .await?;
    match exit_code {
        1 => Ok(Vec::new()),
        0 => Ok(parse_configured_filter_names(&stdout)),
        _ => Err(git_execution_error(&stderr)),
    }
}

pub(crate) fn parse_configured_filter_names(output: &[u8]) -> Vec<String> {    let mut names: Vec<String> = Vec::new();
    for line in output.split(|byte| *byte == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("");
        let key = line.split(' ').next().unwrap_or("");
        let Some(name) = key
            .strip_prefix("filter.")
            .and_then(|rest| {
                rest.strip_suffix(".clean")
                    .or_else(|| rest.strip_suffix(".process"))
            })
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

pub(crate) fn filter_override_args(names: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(names.len() * 6);
    for name in names {
        // `cat` keeps worktree content byte-identical: an identity clean filter
        // means the inventory diff compares raw worktree bytes with no conversion
        // and no repository-defined code. The process protocol rejects `cat`
        // during its handshake, so Git falls back to the clean command, and a
        // non-required filter keeps any fallback silent and non-fatal.
        args.push("-c".into());
        args.push(format!("filter.{name}.clean=cat"));
        args.push("-c".into());
        args.push(format!("filter.{name}.process=cat"));
        args.push("-c".into());
        args.push(format!("filter.{name}.required=false"));
    }
    args
}

pub(crate) async fn reject_applicable_filters(
    root: &Path,
    files: &[String],
    configured: &[String],
) -> Result<(), CommandError> {
    if configured.is_empty() || files.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for file in files {
        input.extend_from_slice(file.as_bytes());
        input.push(0);
    }
    let (exit_code, stdout, stderr) = run_git_with_stdin(
        repo_stdin_git_command(root, &["check-attr", "-z", "--stdin", "filter"]),
        input,
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let applicable = parse_applicable_filter_paths(&stdout, configured);
    if applicable.is_empty() {
        return Ok(());
    }
    Err(unsupported_filter_error())
}

pub(crate) fn parse_applicable_filter_paths(output: &[u8], configured: &[String]) -> Vec<String> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut applicable = Vec::new();
    while let Some(path) = fields.next() {
        let Some(attribute) = fields.next() else {
            break;
        };
        let Some(value) = fields.next() else {
            break;
        };
        if attribute != b"filter" {
            continue;
        }
        let Ok(value) = std::str::from_utf8(value) else {
            continue;
        };
        if matches!(value, "unspecified" | "set" | "unset") {
            continue;
        }
        if configured.iter().any(|name| name == value) {
            if let Ok(path) = std::str::from_utf8(path) {
                applicable.push(path.to_string());
            }
        }
    }
    applicable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{review_changes, review_patch};
    use crate::testutil::{test_git, test_pool, test_repo};
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    #[test]
    fn parses_configured_filter_names() {
        assert_eq!(
            parse_configured_filter_names(
                b"filter.lfs.clean git-lfs clean -- %f\nfilter.lfs.process git-lfs filter-process\nfilter.lfs.process git-lfs filter-process\nother.key value\n"
            ),
            ["lfs"]
        );
        assert!(parse_configured_filter_names(b"filter..clean x\n").is_empty());
        assert!(parse_configured_filter_names(b"filter.lfs.smudge x\n").is_empty());
        assert!(parse_configured_filter_names(b"").is_empty());
    }
    #[test]
    fn builds_filter_override_arguments() {
        assert_eq!(filter_override_args(&[]), Vec::<String>::new());
        assert_eq!(
            filter_override_args(&["lfs".into()]),
            [
                "-c",
                "filter.lfs.clean=cat",
                "-c",
                "filter.lfs.process=cat",
                "-c",
                "filter.lfs.required=false"
            ]
        );
    }
    #[test]
    fn parses_applicable_filter_paths() {
        let configured = ["lfs".to_string()];
        assert_eq!(
            parse_applicable_filter_paths(
                b"a.bin\0filter\0lfs\0b.txt\0filter\0unspecified\0c.txt\0filter\0set\0d.txt\0filter\0unset\0",
                &configured
            ),
            ["a.bin"]
        );
        assert!(parse_applicable_filter_paths(b"a.txt\0filter\0crypt\0", &configured).is_empty());
        assert!(parse_applicable_filter_paths(b"trailing\0filter\0", &configured).is_empty());
    }

    #[tokio::test]
    async fn filters_only_refuse_matching_changed_files() {
        let repo = test_repo("applicable-filter-scope");
        // Add and commit before configuring the filter: `git add` and commit-time
        // index refresh both run clean filters, and this one is unrunnable.
        std::fs::write(repo.join("blob.mark"), "base\n").unwrap();
        std::fs::write(repo.join("plain.txt"), "base\n").unwrap();
        test_git(&repo, &["add", "blob.mark"]);
        test_git(&repo, &["add", "plain.txt"]);
        std::fs::write(repo.join(".gitattributes"), "*.mark filter=marker\n").unwrap();
        test_git(&repo, &["add", ".gitattributes"]);
        test_git(&repo, &["commit", "--quiet", "-m", "attributes"]);
        test_git(&repo, &["config", "filter.marker.required", "true"]);
        test_git(
            &repo,
            &["config", "filter.marker.clean", "wtv-missing-filter-command"],
        );
        std::fs::write(repo.join("plain.txt"), "changed\n").unwrap();

        // With the filter-matching blob.mark untouched, the unrelated change
        // reviews normally despite the configured filter.
        let pool = test_pool().await;
        let review = review_changes(
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
        assert!(review.files.iter().any(|file| file.path == "plain.txt"));
        assert!(!review.files.iter().any(|file| file.path == "blob.mark"));

        let patch = review_patch(
            &pool,
repo.to_str().unwrap().into(),
            None,
            "HEAD".into(),
            None,
            false,
            false,
            "plain.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("+changed"));

        std::fs::write(repo.join("blob.mark"), "changed\n").unwrap();

        let refused = review_changes(
            &pool,
            repo.to_str().unwrap(),
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code, "git_filter_unsupported");

        let patch_error = review_patch(
            &pool,
repo.to_str().unwrap().into(),
            None,
            "HEAD".into(),
            None,
            false,
            false,
            "blob.mark".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(patch_error.code, "git_filter_unsupported");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn stdin_diffs_ignore_repository_and_configured_attributes() {
        const CHILD_ENV: &str = "WORKTREEVIEW_ATTRIBUTE_ISOLATION_CHILD";
        const REPO_ENV: &str = "WORKTREEVIEW_ATTRIBUTE_ISOLATION_REPO";
        const ATTRS_ENV: &str = "WORKTREEVIEW_ATTRIBUTE_ISOLATION_ATTRS";

        if std::env::var_os(CHILD_ENV).is_some() {
            let repo = PathBuf::from(std::env::var_os(REPO_ENV).unwrap());
            let configured_attributes = PathBuf::from(std::env::var_os(ATTRS_ENV).unwrap());
            let pool = test_pool().await;

            let force_binary = "* binary\n- binary\ntext.txt binary\n";
            std::fs::write(repo.join(".gitattributes"), force_binary).unwrap();
            std::fs::write(&configured_attributes, force_binary).unwrap();
            let text = review_patch(
                &pool,
repo.to_str().unwrap().into(),
                None,
                "HEAD".into(),
                None,
                false,
                false,
                "text.txt".into(),
                true,
            )
            .await
            .unwrap();
            assert!(!text.binary);
            assert!(text.text.contains("+plain text"));

            let force_text = "* text\n- text\nbinary.bin text\n";
            std::fs::write(repo.join(".gitattributes"), force_text).unwrap();
            std::fs::write(&configured_attributes, force_text).unwrap();
            let binary = review_patch(
                &pool,
repo.to_str().unwrap().into(),
                None,
                "HEAD".into(),
                None,
                false,
                false,
                "binary.bin".into(),
                true,
            )
            .await
            .unwrap();
            assert!(binary.binary);
            assert!(binary.text.is_empty());
            return;
        }

        let repo = test_repo("attribute-isolation");
        std::fs::write(repo.join("text.txt"), "plain text\n").unwrap();
        std::fs::write(repo.join("binary.bin"), [b'a', 0, b'b']).unwrap();
        let configured_attributes = repo.join("configured-attributes");
        std::fs::write(&configured_attributes, "* binary\n").unwrap();
        let config = repo.join("attributes.gitconfig");
        let attributes_file = configured_attributes.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &config,
            format!("[core]\n\tattributesFile = {attributes_file}\n"),
        )
        .unwrap();

        let attr_source_probe = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["check-attr", "diff", "--", "tracked.txt"])
            .env("GIT_ATTR_SOURCE", "refs/heads/worktreeview-missing")
            .output()
            .unwrap();
        let mut child = StdCommand::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "git::filters::tests::stdin_diffs_ignore_repository_and_configured_attributes",
                "--nocapture",
            ])
            .current_dir(&repo)
            .env(CHILD_ENV, "1")
            .env(REPO_ENV, &repo)
            .env(ATTRS_ENV, &configured_attributes)
            .env("GIT_CONFIG_GLOBAL", &config)
            .env("GIT_CONFIG_SYSTEM", &config);
        if !attr_source_probe.status.success() {
            child.env("GIT_ATTR_SOURCE", "refs/heads/worktreeview-missing");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "attribute isolation child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn applicable_filters_are_rejected_without_execution() {
        use std::os::unix::fs::PermissionsExt;

        let repo = test_repo("configured-filter");
        let marker = repo.join("filter-ran");
        let helper = repo.join("filter.sh");
        std::fs::write(repo.join(".gitattributes"), "tracked.txt filter=marker\n").unwrap();
        test_git(&repo, &["add", ".gitattributes"]);
        test_git(&repo, &["commit", "--quiet", "-m", "attributes"]);
        std::fs::write(
            &helper,
            format!("#!/bin/sh\n: > '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        test_git(
            &repo,
            &["config", "filter.marker.clean", helper.to_str().unwrap()],
        );
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();

        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "tracked.txt",
            ])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let pool = test_pool().await;
        let index_error = review_changes(
            &pool,
            repo.to_str().unwrap(),
repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(index_error.code, "git_filter_unsupported");
        assert!(!marker.exists());

        let patch_error = review_patch(
            &pool,
repo.to_str().unwrap().into(),
            None,
            "HEAD".into(),
            None,
            false,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(patch_error.code, "git_filter_unsupported");
        assert!(!marker.exists());

        test_git(&repo, &["config", "--unset", "filter.marker.clean"]);
        test_git(
            &repo,
            &["config", "filter.marker.process", helper.to_str().unwrap()],
        );
        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "tracked.txt",
            ])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();
        let process_error = review_changes(
            &pool,
            repo.to_str().unwrap(),
repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(process_error.code, "git_filter_unsupported");
        assert!(!marker.exists());

        std::fs::write(repo.join(".gitattributes"), "other.txt filter=marker\n").unwrap();
        let review = review_changes(
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
        assert!(review.files.iter().any(|file| file.path == "tracked.txt"));
        assert!(review
            .files
            .iter()
            .any(|file| file.path == ".gitattributes"));
        assert!(!marker.exists());

        std::fs::remove_dir_all(repo).unwrap();
    }
}
