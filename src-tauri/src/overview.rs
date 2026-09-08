use crate::git::{
    git_args, git_execution_error, parse_branch_records, parse_worktrees, primary_branch,
    run_git, BranchSummary,
};
use crate::{canonical_path, CommandError};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct BranchInventory {
    pub default_branch: Option<String>,
    pub origin_url: Option<String>,
    pub remote_branch_count: u32,
    pub branches: Vec<BranchSummary>,
}

// One bounded pass over refs/heads gives the project page almost everything:
// per-branch HEAD, last-commit identity, upstream and its ahead/behind track.
// Branches checked out in worktrees without an upstream get one extra
// rev-list --count against the default branch, so agent-created branches
// still show their divergence from main. The optional fallback probe and
// origin URL degrade to empty values on failure without sinking the
// inventory; structural Git failures still error.
// for-each-ref does not reliably expand %xNN hex escapes (that is a git log
// pretty-format feature), so the record and field separators are embedded as
// literal control characters; the spawn passes args without a shell. The
// parser tolerates these bytes appearing inside commit metadata. Subjects may
// contain any character except the separators, mirroring parse_commits.
const BRANCH_FORMAT: &str = "--format=%(refname)\u{1f}%(objectname)\u{1f}%(committerdate:unix)\u{1f}%(authorname)\u{1f}%(contents:subject)\u{1f}%(upstream)\u{1f}%(upstream:track)\u{1e}";

pub(crate) async fn branch_inventory(path: String) -> Result<BranchInventory, CommandError> {
    let path = canonical_path(&path)?;
    let (exit_code, stdout, stderr) =
        run_git(&path, &["for-each-ref", "refs/heads", BRANCH_FORMAT]).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let records = parse_branch_records(&stdout)?;

    let remote_branch_count = remote_branch_count(&path).await?;
    let origin_url = origin_url(&path).await;

    let heads: Vec<String> = records
        .iter()
        .map(|record| record.ref_name.clone())
        .collect();
    let default_branch = primary_branch(&heads).map(str::to_string);

    // The fallback is bounded by the worktree count: only branches actually
    // checked out somewhere get an extra probe.
    let worktree_branches = worktree_branches(&path).await;
    let mut branches = Vec::with_capacity(records.len());
    for record in &records {
        let counts = match record.upstream.as_deref() {
            Some(_) => parse_track(&record.track),
            None => {
                let fallback = default_branch.as_deref().filter(|base| {
                    *base != record.ref_name && worktree_branches.contains(&record.ref_name)
                });
                match fallback {
                    Some(base) => {
                        ahead_behind_vs(&path, base, &record.ref_name)
                            .await
                            .unwrap_or(None)
                    }
                    None => None,
                }
            }
        };
        let (ahead, behind) = match counts {
            Some((ahead, behind)) => (Some(ahead), Some(behind)),
            None => (None, None),
        };
        branches.push(BranchSummary {
            ref_name: record.ref_name.clone(),
            head: record.head.clone(),
            author: record.author.clone(),
            subject: record.subject.clone(),
            commit_date: record.commit_date,
            upstream: record.upstream.clone(),
            ahead,
            behind,
        });
    }
    Ok(BranchInventory {
        default_branch,
        origin_url,
        remote_branch_count,
        branches,
    })
}

// `%(upstream:track)` shapes: "" when in sync, "[ahead N]", "[behind N]",
// "[ahead N, behind M]", "[gone]" when the upstream ref no longer exists.
// Anything unparseable counts as unknown.
fn parse_track(track: &str) -> Option<(u32, u32)> {
    let inner = track.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() {
        return Some((0, 0));
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in inner.split(',') {
        let part = part.trim();
        let Some((kind, count)) = part.split_once(' ') else {
            return None;
        };
        let Ok(count) = count.trim().parse::<u32>() else {
            return None;
        };
        match kind {
            "ahead" => ahead = count,
            "behind" => behind = count,
            _ => return None,
        }
    }
    Some((ahead, behind))
}

async fn ahead_behind_vs(
    path: &Path,
    base: &str,
    branch: &str,
) -> Result<Option<(u32, u32)>, CommandError> {
    let owned_args = vec![
        "rev-list".to_string(),
        "--left-right".to_string(),
        "--count".to_string(),
        format!("{base}...{branch}"),
    ];
    let args = git_args(&owned_args);
    let (exit_code, stdout, _) = run_git(path, &args).await?;
    if exit_code != 0 {
        return Ok(None);
    }
    let text = std::str::from_utf8(&stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid count data."))?;
    let mut parts = text.split_whitespace();
    let behind = parts.next().and_then(|value| value.parse::<u32>().ok());
    let ahead = parts.next().and_then(|value| value.parse::<u32>().ok());
    Ok(match (behind, ahead) {
        (Some(behind), Some(ahead)) => Some((ahead, behind)),
        _ => None,
    })
}

// Counts remote-tracking branches, excluding the symbolic refs/remotes/*/HEAD.
async fn remote_branch_count(path: &Path) -> Result<u32, CommandError> {
    let (exit_code, stdout, stderr) = run_git(
        path,
        &["for-each-ref", "refs/remotes", "--format=%(refname)"],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let text = std::str::from_utf8(&stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid ref data."))?;
    Ok(text
        .lines()
        .filter(|reference| {
            reference.starts_with("refs/remotes/")
                && !reference.ends_with("/HEAD")
                && reference.len() > "refs/remotes/".len()
        })
        .count() as u32)
}

async fn origin_url(path: &Path) -> Option<String> {
    let (exit_code, stdout, _) = run_git(path, &["config", "--get", "remote.origin.url"])
        .await
        .ok()?;
    if exit_code != 0 {
        return None;
    }
    let url = std::str::from_utf8(&stdout).ok()?.trim();
    (!url.is_empty()).then(|| url.to_string())
}

async fn worktree_branches(path: &Path) -> Vec<String> {
    let (exit_code, stdout, _) = match run_git(
        path,
        &[
            "-c",
            "core.quotePath=false",
            "worktree",
            "list",
            "--porcelain",
        ],
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return Vec::new(),
    };
    if exit_code != 0 {
        return Vec::new();
    }
    parse_worktrees(&stdout)
        .unwrap_or_default()
        .into_iter()
        .filter(|worktree| worktree.branch.starts_with("refs/heads/"))
        .map(|worktree| worktree.branch)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{test_git, test_repo};
    use std::process::Command as StdCommand;

    #[test]
    fn track_text_resolves_to_counts() {
        assert_eq!(parse_track(""), Some((0, 0)));
        assert_eq!(parse_track("[ahead 2]"), Some((2, 0)));
        assert_eq!(parse_track("[behind 3]"), Some((0, 3)));
        assert_eq!(parse_track("[ahead 1, behind 4]"), Some((1, 4)));
        assert_eq!(parse_track("[gone]"), None);
        assert_eq!(parse_track("[ahead x]"), None);
        assert_eq!(parse_track("[sideways 2]"), None);
    }

    #[tokio::test]
    async fn inventory_reports_history_sync_and_fallback_counts() {
        let repo = test_repo("inventory-main");
        // The init default branch is environment-dependent; pin it.
        test_git(&repo, &["branch", "-M", "master"]);

        // feature: one commit ahead of main, no upstream; the fallback
        // computes its divergence from the default branch.
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature work"]);
        test_git(&repo, &["checkout", "--quiet", "master"]);
        // main moves on, leaving feature also behind.
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();
        test_git(&repo, &["commit", "--quiet", "-am", "main moves"]);
        // Fake an upstream for master: the remote must exist in config for
        // %(upstream) to resolve, then the tracking ref pins its position.
        test_git(
            &repo,
            &["remote", "add", "origin", repo.to_str().unwrap()],
        );
        test_git(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/master",
                &crate::testutil::test_rev_parse(&repo, "master"),
            ],
        );
        test_git(&repo, &["config", "branch.master.remote", "origin"]);
        test_git(&repo, &["config", "branch.master.merge", "refs/heads/master"]);

        let inventory = branch_inventory(repo.to_str().unwrap().into())
            .await
            .unwrap();
        assert_eq!(inventory.default_branch, Some("refs/heads/master".into()));
        // The fake remote points at the repository itself.
        assert_eq!(
            inventory.origin_url.as_deref(),
            Some(repo.to_str().unwrap())
        );
        assert_eq!(inventory.remote_branch_count, 1);

        let master = inventory
            .branches
            .iter()
            .find(|branch| branch.ref_name == "refs/heads/master")
            .unwrap();
        assert_eq!((master.ahead, master.behind), (Some(0), Some(0)));
        assert_eq!(master.subject, "main moves");
        assert_eq!(master.author, "WorktreeView Tests");
        assert_eq!(
            master.upstream.as_deref(),
            Some("refs/remotes/origin/master")
        );

        let feature = inventory
            .branches
            .iter()
            .find(|branch| branch.ref_name == "refs/heads/feature")
            .unwrap();
        // No upstream and not checked out in any worktree: the fallback
        // probe is bounded to worktree branches, so this stays unknown.
        assert_eq!(feature.upstream, None);
        assert_eq!((feature.ahead, feature.behind), (None, None));
        assert_eq!(feature.subject, "feature work");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn inventory_reads_the_origin_url_when_configured() {
        let repo = test_repo("inventory-origin");
        test_git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/demo.git",
            ],
        );
        let inventory = branch_inventory(repo.to_str().unwrap().into())
            .await
            .unwrap();
        assert_eq!(
            inventory.origin_url.as_deref(),
            Some("https://github.com/example/demo.git")
        );
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn worktree_branches_without_upstream_get_the_fallback() {
        let repo = test_repo("inventory-worktree");
        let linked = crate::testutil::test_path("inventory-worktree-linked");
        std::fs::create_dir(&linked).unwrap();
        test_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "agent/fix",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::write(linked.join("fix.txt"), "fix\n").unwrap();
        test_git(&linked, &["add", "fix.txt"]);
        test_git(&linked, &["commit", "--quiet", "-m", "agent fix"]);

        let inventory = branch_inventory(repo.to_str().unwrap().into())
            .await
            .unwrap();
        let agent = inventory
            .branches
            .iter()
            .find(|branch| branch.ref_name == "refs/heads/agent/fix")
            .unwrap();
        assert_eq!((agent.ahead, agent.behind), (Some(1), Some(0)));

        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", linked.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }
}
