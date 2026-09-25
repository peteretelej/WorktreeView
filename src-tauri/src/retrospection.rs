use crate::git::{git_execution_error, parse_worktrees, read_target, run_git, ReadTarget};
use crate::store::now_millis;
use crate::{plain_path, CommandError};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct GoneSurface {
    kind: String,
    identity_key: String,
    label: String,
    detail: String,
    head_sha: String,
    last_seen_at: i64,
    pinned_at: Option<i64>,
}

// Pin state of one recorded surface, reported for live rows too: gone rows
// carry `pinned_at` themselves, but a surface that is still alive only
// appears here.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct SurfacePinRef {
    kind: String,
    identity_key: String,
    pinned_at: i64,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct SurfaceListing {
    gone: Vec<GoneSurface>,
    pinned: Vec<SurfacePinRef>,
}

// Open-time retrospection: remember the identity and last resolved head of
// every reviewed surface so it can still be listed and opened after it
// disappears. Recording is best-effort like every other cache write: a
// failed upsert never fails the open that produced it. An empty `label`
// means "unknown this open; keep the recorded one".
pub(crate) async fn record_surface_open(
    pool: &SqlitePool,
    repo_path: &str,
    kind: &str,
    identity_key: &str,
    label: &str,
    detail: &str,
    head_sha: &str,
) {
    // A move is a recorded surface whose head changed between opens; a
    // first record narrates nothing. The read races nothing: opens are
    // human-paced and the upsert below is the only head writer.
    let previous: Option<Option<String>> = sqlx::query_scalar(
        "SELECT head_sha FROM retrospected_surfaces \
         WHERE repo_path = ? AND kind = ? AND identity_key = ?",
    )
    .bind(repo_path)
    .bind(kind)
    .bind(identity_key)
    .fetch_optional(pool)
    .await
    .ok();
    let _ = sqlx::query(
        "INSERT INTO retrospected_surfaces \
         (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (repo_path, kind, identity_key) DO UPDATE SET \
         last_seen_at = excluded.last_seen_at, \
         head_sha = excluded.head_sha, \
         label = CASE WHEN excluded.label = '' \
                      THEN retrospected_surfaces.label ELSE excluded.label END, \
         detail = excluded.detail",
    )
    .bind(repo_path)
    .bind(kind)
    .bind(identity_key)
    .bind(label)
    .bind(detail)
    .bind(head_sha)
    .bind(now_millis())
    .execute(pool)
    .await;
    if previous.flatten().is_some_and(|previous| previous != head_sha) {
        let recorded = crate::events::record_event(
            pool,
            crate::events::NewEvent {
                repo_path: repo_path.to_string(),
                kind: crate::events::SURFACE_HEAD_MOVED,
                base_sha: None,
                target_key: Some(identity_key.to_string()),
                target_kind: Some(kind.to_string()),
                request_id: None,
                comment_id: None,
                actor_kind: crate::events::ACTOR_HUMAN,
                actor_name: "human".to_string(),
                summary: format!("head moved to {}", crate::requests::short_sha(head_sha)),
            },
        )
        .await;
        // The recording contract is best-effort end to end: this cache
        // write has no error channel to fail, so narration failure only
        // logs.
        if let Err(error) = recorded {
            log::warn!("head-move event for {identity_key} was not recorded: {error}");
        }
    }
}

// Gone detection reads live inventory on every listing; nothing here is
// cached and listing never inserts rows.
pub(crate) async fn list_surfaces_in_pool(
    pool: &SqlitePool,
    path: &str,
) -> Result<SurfaceListing, CommandError> {
    match read_target(pool, path).await? {
        ReadTarget::Local(repo) => {
            let repo_path = repo.to_str().ok_or_else(|| {
                CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
            })?;
            let (exit_code, stdout, stderr) = run_git(
                &repo,
                &[
                    "-c",
                    "core.quotePath=false",
                    "worktree",
                    "list",
                    "--porcelain",
                ],
            )
            .await?;
            if exit_code != 0 {
                return Err(git_execution_error(&stderr));
            }
            let mut live_worktrees = HashSet::new();
            for worktree in parse_worktrees(&stdout)? {
                live_worktrees.insert(live_worktree_key(&worktree.path));
            }
            let (exit_code, stdout, stderr) = run_git(
                &repo,
                &["for-each-ref", "refs/heads", "refs/remotes", "--format=%(refname)"],
            )
            .await?;
            if exit_code != 0 {
                return Err(git_execution_error(&stderr));
            }
            let live_branch_refs = parse_refnames(&stdout)?;
            classify_surfaces(pool, repo_path, live_worktrees, live_branch_refs).await
        }
        ReadTarget::Remote(target) => {
            // Both live inventories ride one batched invocation; the store
            // rows key on the remote identity string.
            let batch = crate::git::run_remote_batch(
                &target,
                &[
                    crate::git::worktree_list_fragment(),
                    crate::git::batch_fragment(vec![
                        "for-each-ref".into(),
                        "refs/heads".into(),
                        "refs/remotes".into(),
                        "--format=%(refname)".into(),
                    ]),
                ],
            )
            .await?;
            let mut live_worktrees = HashSet::new();
            for worktree in parse_worktrees(&batch.ok(0)?.stdout)? {
                // Remote worktree paths are host-side POSIX paths; they are
                // compared in their raw form because local canonicalization
                // cannot resolve them.
                live_worktrees.insert(worktree.path);
            }
            let live_branch_refs = parse_refnames(&batch.ok(1)?.stdout)?;
            classify_surfaces(pool, path, live_worktrees, live_branch_refs).await
        }
    }
}

fn parse_refnames(stdout: &[u8]) -> Result<HashSet<String>, CommandError> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid ref data."))?;
    Ok(text.lines().map(str::to_string).collect())
}

async fn classify_surfaces(
    pool: &SqlitePool,
    repo_path: &str,
    live_worktrees: HashSet<String>,
    live_branch_refs: HashSet<String>,
) -> Result<SurfaceListing, CommandError> {
    // Branch pins can point at remote-tracking refs, so a ref counts as live
    // when either side of the inventory still carries it.
    let rows = sqlx::query(
        "SELECT kind, identity_key, label, detail, head_sha, last_seen_at, pinned_at \
         FROM retrospected_surfaces WHERE repo_path = ? \
         ORDER BY last_seen_at DESC, kind ASC, identity_key ASC",
    )
    .bind(repo_path)
    .fetch_all(pool)
    .await?;
    let mut gone = Vec::new();
    let mut pinned = Vec::new();
    for row in rows {
        let kind: String = row.try_get("kind")?;
        let identity_key: String = row.try_get("identity_key")?;
        let pinned_at: Option<i64> = row.try_get("pinned_at")?;
        if let Some(pinned_at) = pinned_at {
            pinned.push(SurfacePinRef {
                kind: kind.clone(),
                identity_key: identity_key.clone(),
                pinned_at,
            });
        }
        let live = match kind.as_str() {
            "worktree" => live_worktrees.contains(&identity_key),
            "branch" => live_branch_refs.contains(&identity_key),
            _ => false,
        };
        if !live {
            gone.push(GoneSurface {
                kind,
                identity_key,
                label: row.try_get("label")?,
                detail: row.try_get("detail")?,
                head_sha: row.try_get("head_sha")?,
                last_seen_at: row.try_get("last_seen_at")?,
                pinned_at,
            });
        }
    }
    Ok(SurfaceListing { gone, pinned })
}

// Store-only head inventory for the attention queue: the recorded identity
// and last resolved head of every retrospected surface. No Git runs here;
// rows reflect the last retrospection pass, which the queue's
// progressive-disclosure contract accepts.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct SurfaceHead {
    pub(crate) repo_path: String,
    pub(crate) kind: String,
    pub(crate) identity_key: String,
    pub(crate) label: String,
    pub(crate) head_sha: String,
    pub(crate) last_seen_at: i64,
}

pub(crate) async fn recorded_surface_heads_in_pool(
    pool: &SqlitePool,
) -> Result<Vec<SurfaceHead>, sqlx::Error> {
    sqlx::query(
        "SELECT repo_path, kind, identity_key, label, head_sha, last_seen_at \
         FROM retrospected_surfaces \
         ORDER BY last_seen_at DESC, kind ASC, identity_key ASC",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| {
        Ok(SurfaceHead {
            repo_path: row.try_get("repo_path")?,
            kind: row.try_get("kind")?,
            identity_key: row.try_get("identity_key")?,
            label: row.try_get("label")?,
            head_sha: row.try_get("head_sha")?,
            last_seen_at: row.try_get("last_seen_at")?,
        })
    })
    .collect()
}

// A request's worktree target joins a recorded surface through the same
// canonicalization the recording path used; a key that no longer resolves
// (the surface is gone) compares in its raw form as a fallback.
pub(crate) fn recorded_worktree_head<'a>(
    heads: &'a [SurfaceHead],
    repo_path: &str,
    target_key: &str,
) -> Option<&'a SurfaceHead> {
    let key = live_worktree_key(target_key);
    heads.iter().find(|head| {
        head.kind == "worktree"
            && head.repo_path == repo_path
            && (head.identity_key == key || head.identity_key == target_key)
    })
}

// Recorded worktree keys are platform-canonical plain paths while
// `worktree list --porcelain` emits forward-slash paths; run every live
// path through the same canonicalization before comparing. A live path
// that fails canonicalization (vanishing between listing and conversion)
// is compared in its raw form as a fallback.
fn live_worktree_key(path: &str) -> String {
    Path::new(path)
        .canonicalize()
        .map(|canonical| plain_path(&canonical).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

// Pin or unpin one surface. An already-recorded surface (live or gone) is
// pinned in place with no spawn; only an unknown identity resolves fresh,
// with at most one bounded spawn (one batched invocation remotely). Unpinning
// deletes pin-created rows (a pin is the only thing holding them) and just
// clears review-recorded rows.
pub(crate) async fn set_surface_pinned_in_pool(
    pool: &SqlitePool,
    path: &str,
    kind: &str,
    identity_key: &str,
    pinned: bool,
) -> Result<Option<i64>, CommandError> {
    match read_target(pool, path).await? {
        ReadTarget::Local(repo) => {
            let repo_path = repo.to_str().ok_or_else(|| {
                CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
            })?;
            let identity_key = match kind {
                "worktree" => live_worktree_key(identity_key),
                _ => identity_key.to_string(),
            };
            set_surface_pinned(
                pool,
                repo_path,
                kind,
                &identity_key,
                pinned,
                SurfaceIdentity::Local(&repo),
            )
            .await
        }
        ReadTarget::Remote(target) => {
            // Remote worktree keys are host-side POSIX paths, compared raw.
            set_surface_pinned(
                pool,
                path,
                kind,
                identity_key,
                pinned,
                SurfaceIdentity::Remote(&target),
            )
            .await
        }
    }
}

// Where a not-yet-recorded surface's identity resolves: live inventory over
// Git on the local checkout or, batched, on the host.
enum SurfaceIdentity<'a> {
    Local(&'a Path),
    Remote(&'a crate::git::RemoteTarget),
}

async fn set_surface_pinned(
    pool: &SqlitePool,
    repo_path: &str,
    kind: &str,
    identity_key: &str,
    pinned: bool,
    resolver: SurfaceIdentity<'_>,
) -> Result<Option<i64>, CommandError> {
    let persisted_error = || {
        CommandError::new(
            "persistence",
            "The surface is not recorded and cannot be pinned.",
        )
    };
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT origin FROM retrospected_surfaces \
         WHERE repo_path = ? AND kind = ? AND identity_key = ?",
    )
    .bind(repo_path)
    .bind(kind)
    .bind(identity_key)
    .fetch_optional(pool)
    .await?;
    let pinned_at = pinned.then(now_millis);
    if let Some(origin) = existing {
        if pinned {
            sqlx::query(
                "UPDATE retrospected_surfaces SET pinned_at = ? \
                 WHERE repo_path = ? AND kind = ? AND identity_key = ?",
            )
            .bind(pinned_at)
            .bind(repo_path)
            .bind(kind)
            .bind(identity_key)
            .execute(pool)
            .await?;
        } else if origin == "pin" {
            sqlx::query(
                "DELETE FROM retrospected_surfaces \
                 WHERE repo_path = ? AND kind = ? AND identity_key = ?",
            )
            .bind(repo_path)
            .bind(kind)
            .bind(identity_key)
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE retrospected_surfaces SET pinned_at = NULL \
                 WHERE repo_path = ? AND kind = ? AND identity_key = ?",
            )
            .bind(repo_path)
            .bind(kind)
            .bind(identity_key)
            .execute(pool)
            .await?;
        }
        return Ok(pinned_at);
    }
    if !pinned {
        return Err(persisted_error());
    }
    let (label, detail, head_sha) =
        resolve_surface_identity(&resolver, kind, identity_key).await?;
    sqlx::query(
        "INSERT INTO retrospected_surfaces \
         (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at, pinned_at, origin) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pin')",
    )
    .bind(repo_path)
    .bind(kind)
    .bind(identity_key)
    .bind(&label)
    .bind(&detail)
    .bind(&head_sha)
    .bind(now_millis())
    .bind(pinned_at)
    .execute(pool)
    .await?;
    Ok(pinned_at)
}

// Pin-time identity for a surface the app never recorded. Branches resolve
// with one rev-parse; worktrees come from one worktree listing that also
// carries the branch label and head.
async fn resolve_surface_identity(
    repo: &SurfaceIdentity<'_>,
    kind: &str,
    identity_key: &str,
) -> Result<(String, String, String), CommandError> {
    if identity_key.starts_with('-') {
        return Err(CommandError::new(
            "persistence",
            "The surface is not recorded and cannot be pinned.",
        ));
    }
    let not_recorded = || {
        CommandError::new(
            "persistence",
            format!("The surface '{identity_key}' is not recorded and cannot be pinned."),
        )
    };
    match (kind, repo) {
        ("branch", SurfaceIdentity::Local(path)) => {
            let (exit_code, stdout, _stderr) =
                run_git(path, &["rev-parse", "--verify", identity_key]).await?;
            if exit_code != 0 {
                return Err(not_recorded());
            }
            let head_sha = std::str::from_utf8(&stdout)
                .map_err(|_| {
                    CommandError::new("git_output_malformed", "Git returned invalid ref data.")
                })?
                .trim();
            if head_sha.is_empty() {
                return Err(not_recorded());
            }
            Ok((
                branch_label(identity_key),
                identity_key.to_string(),
                head_sha.to_string(),
            ))
        }
        ("branch", SurfaceIdentity::Remote(target)) => {
            let batch = crate::git::run_remote_batch(
                target,
                &[crate::git::batch_fragment(vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    identity_key.into(),
                ])],
            )
            .await?;
            let fragment = batch.ok(0)?;
            let head_sha = String::from_utf8_lossy(&fragment.stdout).trim().to_string();
            if head_sha.is_empty() {
                return Err(not_recorded());
            }
            Ok((
                branch_label(identity_key),
                identity_key.to_string(),
                head_sha,
            ))
        }
        ("worktree", SurfaceIdentity::Local(path)) => {
            let (exit_code, stdout, stderr) = run_git(
                path,
                &[
                    "-c",
                    "core.quotePath=false",
                    "worktree",
                    "list",
                    "--porcelain",
                ],
            )
            .await?;
            if exit_code != 0 {
                return Err(git_execution_error(&stderr));
            }
            for worktree in parse_worktrees(&stdout)? {
                if live_worktree_key(&worktree.path) == identity_key {
                    return Ok((
                        worktree_label(&worktree.branch),
                        identity_key.to_string(),
                        worktree.head,
                    ));
                }
            }
            Err(not_recorded())
        }
        ("worktree", SurfaceIdentity::Remote(target)) => {
            let batch =
                crate::git::run_remote_batch(target, &[crate::git::worktree_list_fragment()])
                    .await?;
            for worktree in parse_worktrees(&batch.ok(0)?.stdout)? {
                if worktree.path == identity_key {
                    return Ok((
                        worktree_label(&worktree.branch),
                        identity_key.to_string(),
                        worktree.head,
                    ));
                }
            }
            Err(not_recorded())
        }
        _ => Err(not_recorded()),
    }
}

fn branch_label(ref_name: &str) -> String {
    ref_name
        .strip_prefix("refs/heads/")
        .or_else(|| ref_name.strip_prefix("refs/remotes/"))
        .unwrap_or(ref_name)
        .to_string()
}

fn worktree_label(branch: &str) -> String {
    branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string()
}

// Keep `Worktree`'s fields `pub(crate)` in `git::parse.rs` so retrospection
// can read them; no accessor layer needed.

// Re-key repo-scoped retrospection rows when normalize_stored_paths
// rewrites a verbatim path, matching the cache carry's precedent: plain
// rows win if both forms exist.
pub(crate) async fn carry_repo_path(
    pool: &SqlitePool,
    old_path: &str,
    new_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO retrospected_surfaces \
         (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at, pinned_at, origin) \
         SELECT ?, kind, identity_key, label, detail, head_sha, last_seen_at, pinned_at, origin \
         FROM retrospected_surfaces WHERE repo_path = ?",
    )
    .bind(new_path)
    .bind(old_path)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM retrospected_surfaces WHERE repo_path = ?")
        .bind(old_path)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::spawn_counted;
    use crate::review::{commit_page, review_changes};
    use crate::testutil::{seed_repo, test_git, test_path, test_pool, test_repo, test_rev_parse};

    type SurfaceRow = (String, String, String, String, i64);

    async fn surface_rows(pool: &SqlitePool, repo_path: &str) -> Vec<SurfaceRow> {
        sqlx::query(
            "SELECT kind, identity_key, label, head_sha, last_seen_at \
             FROM retrospected_surfaces WHERE repo_path = ?",
        )
        .bind(repo_path)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get("kind"),
                row.get("identity_key"),
                row.get("label"),
                row.get("head_sha"),
                row.get("last_seen_at"),
            )
        })
        .collect()
    }

    fn canonical_key(path: &Path) -> String {
        plain_path(&path.canonicalize().unwrap())
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn recorded_surface_heads_read_store_only_and_join_worktree_keys() {
        let repo = test_repo("surface-heads");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        let head_sha = test_rev_parse(&repo, "HEAD");
        let worktree = test_path("surface-heads-wt");
        test_git(
            &repo,
            &["worktree", "add", "-b", "feature", worktree.to_str().unwrap()],
        );
        let worktree_key = canonical_key(&worktree);
        record_surface_open(
            &pool,
            &repo_path,
            "worktree",
            &worktree_key,
            "feature",
            &worktree_key,
            &head_sha,
        )
        .await;
        record_surface_open(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/feature",
            "feature",
            "refs/heads/feature",
            &head_sha,
        )
        .await;

        // The queue's head read spawns no Git at all: it is a plain store
        // query over the rows the retrospection passes already recorded.
        let (spawns, heads) =
            spawn_counted(recorded_surface_heads_in_pool(&pool)).await;
        assert_eq!(spawns, 0);
        let heads = heads.unwrap();
        assert_eq!(heads.len(), 2);
        let recorded = heads
            .iter()
            .find(|head| head.kind == "worktree")
            .unwrap();
        assert_eq!(recorded.repo_path, repo_path);
        assert_eq!(recorded.identity_key, worktree_key);
        assert_eq!(recorded.label, "feature");
        assert_eq!(recorded.head_sha, head_sha);

        // The request-side join canonicalizes the target key the same way
        // the recording path did; branch rows and unknown keys never match.
        assert!(recorded_worktree_head(&heads, &repo_path, &worktree_key).is_some());
        assert!(recorded_worktree_head(&heads, &repo_path, "refs/heads/feature").is_none());
        assert!(
            recorded_worktree_head(&heads, &repo_path, &format!("{worktree_key}-gone"))
                .is_none()
        );
        assert!(recorded_worktree_head(&heads, "other-repo", &worktree_key).is_none());

        test_git(&repo, &["worktree", "remove", worktree.to_str().unwrap()]);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn gone_worktree_lifecycle_lists_and_reopens() {
        let repo = test_repo("retrospect-worktree");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        let worktree = test_path("retrospect-worktree-wt");
        test_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let worktree_path = worktree.to_str().unwrap().to_string();
        let head_sha = test_rev_parse(&worktree, "HEAD");

        // Opening the worktree's HEAD-based history records its identity and
        // caches the page; against live inventory nothing is gone yet.
        let open = commit_page(
            &pool,
            &repo_path,
            worktree_path.clone(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(list_surfaces_in_pool(&pool, &repo_path)
            .await
            .unwrap()
            .gone
            .is_empty());

        let worktree_key = canonical_key(&worktree);
        test_git(&repo, &["worktree", "remove", &worktree_path]);
        test_git(&repo, &["branch", "-D", "feature"]);
        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].kind, "worktree");
        assert_eq!(listing.gone[0].label, "feature");
        assert_eq!(listing.gone[0].head_sha, head_sha);
        assert_eq!(listing.gone[0].identity_key, worktree_key);

        // Reopening by the recorded head serves the cached page with no git
        // at all, even though the surface no longer exists.
        let (spawns, reopened) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(head_sha),
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 0);
        assert_eq!(reopened.unwrap(), open);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn gone_branch_lifecycle_lists_and_reopens() {
        let repo = test_repo("retrospect-branch");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature-only.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature commit"]);
        test_git(&repo, &["checkout", "--quiet", "main"]);
        let feature_sha = test_rev_parse(&repo, "refs/heads/feature");

        let open = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(list_surfaces_in_pool(&pool, &repo_path)
            .await
            .unwrap()
            .gone
            .is_empty());

        test_git(&repo, &["branch", "-D", "feature"]);
        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].kind, "branch");
        assert_eq!(listing.gone[0].identity_key, "refs/heads/feature");
        assert_eq!(listing.gone[0].label, "feature");
        assert_eq!(listing.gone[0].head_sha, feature_sha);

        let (spawns, reopened) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(feature_sha),
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 0);
        assert_eq!(reopened.unwrap(), open);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn recording_shape_matches_open_kinds() {
        let repo = test_repo("retrospect-shape");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        let repo_key = canonical_key(&repo);
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["tag", "v1"]);
        std::fs::write(repo.join("tracked.txt"), "change\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "second"]);
        let head_sha = test_rev_parse(&repo, "HEAD");

        // A HEAD-based open records the worktree surface, labeled from the
        // page's HEAD -> decoration.
        commit_page(&pool, &repo_path, repo_path.clone(), None, None, None, None)
            .await
            .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "worktree");
        assert_eq!(rows[0].1, repo_key);
        assert_eq!(rows[0].2, "main");
        assert_eq!(rows[0].3, head_sha);

        // A branch open records the branch surface under the full ref.
        commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/main".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.0 == "branch"
            && row.1 == "refs/heads/main"
            && row.2 == "main"
            && row.3 == head_sha));

        // A detached worktree open records an empty label (no HEAD ->
        // decoration), so a later nicer label would be kept by the CASE. The
        // worktree gets its own commit so the open fetches freshly in the
        // detached worktree instead of hitting a cached page that carries
        // another worktree's decoration.
        let detached = test_path("retrospect-shape-detached");
        test_git(
            &repo,
            &["worktree", "add", "--detach", detached.to_str().unwrap()],
        );
        let detached_path = detached.to_str().unwrap().to_string();
        std::fs::write(detached.join("detached.txt"), "detached\n").unwrap();
        test_git(&detached, &["add", "detached.txt"]);
        test_git(&detached, &["commit", "--quiet", "-m", "detached commit"]);
        commit_page(
            &pool,
            &repo_path,
            detached_path.clone(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        assert_eq!(rows.len(), 3);
        let detached_row = rows
            .iter()
            .find(|row| row.0 == "worktree" && row.1 == canonical_key(&detached))
            .unwrap();
        assert_eq!(detached_row.2, "");

        // Tag and raw-SHA start refs are not surfaces: they record nothing.
        commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/tags/v1".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(head_sha.clone()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(surface_rows(&pool, &repo_path).await.len(), 3);

        // A review open refreshes last_seen_at and keeps the recorded label
        // when the newer open has none; only the reviewed surface refreshes.
        sqlx::query("UPDATE retrospected_surfaces SET last_seen_at = 1000")
            .execute(&pool)
            .await
            .unwrap();
        review_changes(
            &pool,
            &repo_path,
            repo_path.clone(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        review_changes(
            &pool,
            &repo_path,
            repo_path.clone(),
            "refs/heads/main".into(),
            Some("refs/heads/main".into()),
            true,
            false,
        )
        .await
        .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        let worktree_row = rows
            .iter()
            .find(|row| row.0 == "worktree" && row.1 == repo_key)
            .unwrap();
        assert_eq!(worktree_row.2, "main");
        assert!(worktree_row.4 > 1000);
        let branch_row = rows.iter().find(|row| row.0 == "branch").unwrap();
        assert_eq!(branch_row.2, "main");
        assert!(branch_row.4 > 1000);
        let detached_row = rows
            .iter()
            .find(|row| row.0 == "worktree" && row.1 != repo_key)
            .unwrap();
        assert_eq!(detached_row.4, 1000);

        test_git(&repo, &["worktree", "remove", &detached_path]);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn surface_pin_round_trip_pins_and_unpins_worktree_and_branch() {
        let repo = test_repo("pin-roundtrip");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        let worktree = test_path("pin-roundtrip-wt");
        test_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt-feature",
                worktree.to_str().unwrap(),
            ],
        );
        test_git(&repo, &["branch", "br-feature"]);
        let head_sha = test_rev_parse(&repo, "refs/heads/br-feature");

        // Never-recorded surfaces resolve fresh with one bounded spawn each.
        let (worktree_spawns, worktree_pinned_at) = spawn_counted(set_surface_pinned_in_pool(
            &pool,
            &repo_path,
            "worktree",
            worktree.to_str().unwrap(),
            true,
        ))
        .await;
        assert_eq!(worktree_spawns, 1);
        let (branch_spawns, branch_pinned_at) = spawn_counted(set_surface_pinned_in_pool(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/br-feature",
            true,
        ))
        .await;
        assert_eq!(branch_spawns, 1);

        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert!(listing.gone.is_empty());
        assert_eq!(listing.pinned.len(), 2);
        let worktree_key = canonical_key(&worktree);
        assert!(listing.pinned.contains(&SurfacePinRef {
            kind: "worktree".into(),
            identity_key: worktree_key.clone(),
            pinned_at: worktree_pinned_at.unwrap().unwrap(),
        }));
        assert!(listing.pinned.contains(&SurfacePinRef {
            kind: "branch".into(),
            identity_key: "refs/heads/br-feature".into(),
            pinned_at: branch_pinned_at.unwrap().unwrap(),
        }));
        let branch_row = sqlx::query(
            "SELECT label, detail, head_sha, origin FROM retrospected_surfaces \
             WHERE kind = 'branch' AND identity_key = 'refs/heads/br-feature'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(branch_row.get::<String, _>("label"), "br-feature");
        assert_eq!(
            branch_row.get::<String, _>("detail"),
            "refs/heads/br-feature"
        );
        assert_eq!(branch_row.get::<String, _>("head_sha"), head_sha);
        assert_eq!(branch_row.get::<String, _>("origin"), "pin");

        // Unpinning deletes pin-created rows outright.
        assert_eq!(
            set_surface_pinned_in_pool(&pool, &repo_path, "worktree", &worktree_key, false)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/br-feature", false)
                .await
                .unwrap(),
            None
        );
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retrospected_surfaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);

        test_git(&repo, &["worktree", "remove", worktree.to_str().unwrap()]);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn pinning_recorded_gone_surface_sets_pinned_at_without_spawning() {
        let repo = test_repo("pin-gone");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        record_surface_open(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/dead",
            "dead",
            "refs/heads/dead",
            &"a".repeat(40),
        )
        .await;

        let (spawns, pinned) = spawn_counted(set_surface_pinned_in_pool(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/dead",
            true,
        ))
        .await;
        assert_eq!(spawns, 0);
        let pinned_at = pinned.unwrap();

        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].pinned_at, pinned_at);
        assert_eq!(listing.pinned.len(), 1);
        assert_eq!(listing.pinned[0].identity_key, "refs/heads/dead");

        // Unpinning a review-recorded row keeps the row, pin cleared.
        let (unpin_spawns, unpinned) = spawn_counted(set_surface_pinned_in_pool(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/dead",
            false,
        ))
        .await;
        assert_eq!(unpin_spawns, 0);
        assert_eq!(unpinned.unwrap(), None);
        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].pinned_at, None);
        assert!(listing.pinned.is_empty());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn pinning_unknown_identity_fails_with_persistence() {
        let repo = test_repo("pin-unknown");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        test_git(&repo, &["branch", "-M", "main"]);

        let branch_error =
            set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/missing", true)
                .await
                .unwrap_err();
        assert_eq!(branch_error.code, "persistence");
        let missing_worktree = test_path("pin-unknown-missing");
        let worktree_error = set_surface_pinned_in_pool(
            &pool,
            &repo_path,
            "worktree",
            missing_worktree.to_str().unwrap(),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(worktree_error.code, "persistence");
        let unpin_error = set_surface_pinned_in_pool(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/also-missing",
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(unpin_error.code, "persistence");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn pinned_remote_branch_stays_live_while_the_tracking_ref_exists() {
        let origin = test_repo("pin-remote-origin");
        test_git(&origin, &["branch", "-M", "main"]);
        let repo = test_repo("pin-remote-repo");
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        test_git(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/main",
                &crate::testutil::test_rev_parse(&repo, "main"),
            ],
        );
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;

        set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/remotes/origin/main", true)
            .await
            .unwrap();

        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert!(listing.gone.is_empty());
        assert_eq!(listing.pinned.len(), 1);
        assert_eq!(listing.pinned[0].kind, "branch");
        assert_eq!(listing.pinned[0].identity_key, "refs/remotes/origin/main");

        // Once the tracking ref is pruned the pin reads as gone, labelled by
        // its short remote name.
        test_git(&repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].label, "origin/main");

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn pinned_never_reviewed_surface_appears_gone_with_pin_time_identity() {
        let repo = test_repo("pin-gone-fresh");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["branch", "ephemeral"]);
        let head_sha = test_rev_parse(&repo, "refs/heads/ephemeral");

        set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/ephemeral", true)
            .await
            .unwrap();
        test_git(&repo, &["branch", "-D", "ephemeral"]);

        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].kind, "branch");
        assert_eq!(listing.gone[0].label, "ephemeral");
        assert_eq!(listing.gone[0].head_sha, head_sha);
        assert!(listing.gone[0].pinned_at.is_some());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn unpin_deletes_pin_origin_rows_but_keeps_review_origin_rows() {
        let repo = test_repo("pin-origin");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["branch", "reviewed"]);
        test_git(&repo, &["branch", "pinonly"]);
        record_surface_open(
            &pool,
            &repo_path,
            "branch",
            "refs/heads/reviewed",
            "reviewed",
            "refs/heads/reviewed",
            &"a".repeat(40),
        )
        .await;

        set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/reviewed", true)
            .await
            .unwrap();
        set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/pinonly", true)
            .await
            .unwrap();
        set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/reviewed", false)
            .await
            .unwrap();
        set_surface_pinned_in_pool(&pool, &repo_path, "branch", "refs/heads/pinonly", false)
            .await
            .unwrap();

        let identities: Vec<String> =
            sqlx::query_scalar("SELECT identity_key FROM retrospected_surfaces")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(identities, ["refs/heads/reviewed"]);
        let pinned: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT pinned_at FROM retrospected_surfaces")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(pinned, [None]);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn head_moves_narrate_once_per_changed_head() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        // A first record and a same-head re-open narrate nothing; a changed
        // head narrates once, not once per re-open at the same head.
        record_surface_open(
            &pool,
            "/demo",
            "worktree",
            "/wt",
            "feature",
            "/wt",
            &"a".repeat(40),
        )
        .await;
        record_surface_open(
            &pool,
            "/demo",
            "worktree",
            "/wt",
            "feature",
            "/wt",
            &"a".repeat(40),
        )
        .await;
        record_surface_open(
            &pool,
            "/demo",
            "branch",
            "refs/heads/feature",
            "feature",
            "refs/heads/feature",
            &"a".repeat(40),
        )
        .await;
        record_surface_open(
            &pool,
            "/demo",
            "worktree",
            "/wt",
            "feature",
            "/wt",
            &"b".repeat(40),
        )
        .await;
        record_surface_open(
            &pool,
            "/demo",
            "worktree",
            "/wt",
            "feature",
            "/wt",
            &"b".repeat(40),
        )
        .await;

        let rows = crate::events::list_events_in_pool(
            &pool,
            &crate::events::EventQuery {
                since_id: 0,
                repo_path: None,
                limit: 100,
                ascending: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, crate::events::SURFACE_HEAD_MOVED);
        assert_eq!(rows[0].repo_path, "/demo");
        assert_eq!(rows[0].target_key.as_deref(), Some("/wt"));
        assert_eq!(rows[0].target_kind.as_deref(), Some("worktree"));
        assert_eq!(rows[0].base_sha, None);
        assert_eq!(rows[0].actor_kind, crate::events::ACTOR_HUMAN);
        assert_eq!(
            rows[0].summary,
            format!("head moved to {}", crate::requests::short_sha(&"b".repeat(40)))
        );
    }
}
