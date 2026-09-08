use crate::CommandError;
use serde::Serialize;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Worktree {
    pub(crate) path: String,
    pub(crate) branch: String,
    pub(crate) head: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ChangedFile {
    pub(crate) path: String,
    pub(crate) status: String,
    pub(crate) untracked: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub refs: Vec<String>,
    pub parents: Vec<String>,
    pub default_base_ancestor: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct CommitPage {
    pub commits: Vec<CommitInfo>,
    pub has_more: bool,
}

// One `for-each-ref` record over refs/heads. `upstream` is the configured
// upstream refname (None when the branch has none); `track` is the raw
// `%(upstream:track)` text, resolved into ahead/behind counts by the caller.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BranchRecord {
    pub(crate) ref_name: String,
    pub(crate) head: String,
    pub(crate) commit_date: i64,
    pub(crate) author: String,
    pub(crate) subject: String,
    pub(crate) upstream: Option<String>,
    pub(crate) track: String,
}

// The project-page view of a branch: identity plus just enough history and
// sync state for the worktree table. ahead/behind are None when unknown (no
// upstream and no fallback base, or a gone upstream).
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct BranchSummary {
    pub(crate) ref_name: String,
    pub(crate) head: String,
    pub(crate) author: String,
    pub(crate) subject: String,
    pub(crate) commit_date: i64,
    pub(crate) upstream: Option<String>,
    pub(crate) ahead: Option<u32>,
    pub(crate) behind: Option<u32>,
}

pub(crate) fn parse_untracked_paths(output: &[u8]) -> Result<Vec<String>, CommandError> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path).map(str::to_string).map_err(|_| {
                CommandError::new(
                    "git_output_malformed",
                    "Git returned invalid untracked path data.",
                )
            })
        })
        .collect()
}

// Counts `status --porcelain=v1 -z` entries. With --no-renames every entry is
// exactly one NUL-terminated field, so non-empty segments are the count.
pub(crate) fn parse_status_count(output: &[u8]) -> u32 {
    output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .count() as u32
}

pub(crate) fn parse_name_status(output: &[u8]) -> Result<Vec<ChangedFile>, CommandError> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let status = std::str::from_utf8(status).map_err(|_| {
            CommandError::new("git_output_malformed", "Git returned invalid change data.")
        })?;
        let path = fields.next().ok_or_else(|| {
            CommandError::new(
                "git_output_malformed",
                "Git returned incomplete change data.",
            )
        })?;
        let path = std::str::from_utf8(path).map_err(|_| {
            CommandError::new("git_output_malformed", "Git returned invalid change data.")
        })?;
        let path = if status.starts_with('R') || status.starts_with('C') {
            let new_path = fields.next().ok_or_else(|| {
                CommandError::new(
                    "git_output_malformed",
                    "Git returned incomplete rename data.",
                )
            })?;
            std::str::from_utf8(new_path).map_err(|_| {
                CommandError::new("git_output_malformed", "Git returned invalid change data.")
            })?
        } else {
            path
        };
        files.push(ChangedFile {
            path: path.to_string(),
            status: status.to_string(),
            untracked: false,
        });
    }
    Ok(files)
}

pub(crate) fn parse_numstat(output: &[u8]) -> Result<(i64, i64, bool), CommandError> {
    let malformed = || {
        CommandError::new(
            "git_output_malformed",
            "Git returned malformed numstat data.",
        )
    };
    let mut fields = output.split(|byte| *byte == 0);
    let mut additions = 0;
    let mut deletions = 0;
    let mut binary = false;
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, |byte| *byte == b'\t');
        let added =
            std::str::from_utf8(parts.next().ok_or_else(malformed)?).map_err(|_| malformed())?;
        let deleted =
            std::str::from_utf8(parts.next().ok_or_else(malformed)?).map_err(|_| malformed())?;
        let path = parts.next().ok_or_else(malformed)?;
        let parse_count = |value: &str| {
            if value == "-" {
                Ok(None)
            } else {
                value.parse::<i64>().map(Some).map_err(|_| malformed())
            }
        };
        match (parse_count(added)?, parse_count(deleted)?) {
            (Some(added), Some(deleted)) => {
                additions += added;
                deletions += deleted;
            }
            _ => binary = true,
        }
        if path.is_empty() {
            let old_path = fields.next().ok_or_else(malformed)?;
            let new_path = fields.next().ok_or_else(malformed)?;
            if old_path.is_empty() || new_path.is_empty() {
                return Err(malformed());
            }
            std::str::from_utf8(old_path).map_err(|_| malformed())?;
            std::str::from_utf8(new_path).map_err(|_| malformed())?;
        } else {
            std::str::from_utf8(path).map_err(|_| malformed())?;
        }
    }
    Ok((additions, deletions, binary))
}

// `list_commits` log records: fields joined by \x1f, each record terminated by
// \x1e plus a newline. Subjects may contain any character except the
// separators, so records are never split on lines.
pub(crate) fn parse_commits(output: &[u8]) -> Result<Vec<CommitInfo>, CommandError> {
    let malformed = || {
        CommandError::new(
            "git_output_malformed",
            "Git returned malformed commit data.",
        )
    };
    let text = std::str::from_utf8(output).map_err(|_| malformed())?;
    let mut commits = Vec::new();
    for record in text.split('\u{1e}') {
        // Record edges carry git's record-terminating newlines. Edges are the
        // hex sha and the whitespace-split parents list, so edge whitespace is
        // never meaningful data.
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\u{1f}').collect();
        if fields.len() != 6 || fields[0].is_empty() {
            return Err(malformed());
        }
        commits.push(CommitInfo {
            sha: fields[0].to_string(),
            subject: fields[1].to_string(),
            author: fields[2].to_string(),
            date: fields[3].to_string(),
            refs: if fields[4].is_empty() {
                Vec::new()
            } else {
                fields[4].split(", ").map(str::to_string).collect()
            },
            parents: fields[5].split_whitespace().map(str::to_string).collect(),
            default_base_ancestor: false,
        });
    }
    Ok(commits)
}

// `for-each-ref` records over refs/heads: a fixed five-field prefix (refname,
// sha, committer date, author, subject) followed by upstream and track, all
// joined by \x1f, each record terminated by \x1e. Crafted commits can carry
// the separator bytes inside author or subject, so records with extra fields
// reassemble the subject from the record end; structurally truncated records
// are skipped rather than denying the whole inventory.
pub(crate) fn parse_branch_records(output: &[u8]) -> Result<Vec<BranchRecord>, CommandError> {
    let malformed = || {
        CommandError::new(
            "git_output_malformed",
            "Git returned malformed branch data.",
        )
    };
    let text = std::str::from_utf8(output).map_err(|_| malformed())?;
    let mut branches = Vec::new();
    for record in text.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\u{1f}').collect();
        // A record separator inside commit metadata truncates both halves;
        // dropping those rows beats denying the whole inventory.
        if fields.len() < 7 {
            continue;
        }
        if !fields[0].starts_with("refs/heads/") {
            return Err(malformed());
        }
        let commit_date = fields[2]
            .parse::<i64>()
            .map_err(|_| malformed())?;
        let last = fields.len();
        branches.push(BranchRecord {
            ref_name: fields[0].to_string(),
            head: fields[1].to_string(),
            commit_date,
            author: fields[3].to_string(),
            subject: fields[4..last - 2].join("\u{1f}"),
            upstream: if fields[last - 2].is_empty() {
                None
            } else {
                Some(fields[last - 2].to_string())
            },
            track: fields[last - 1].to_string(),
        });
    }
    Ok(branches)
}

fn decode_git_path(value: &str) -> Result<String, ()> {
    if !value.starts_with('"') {
        return Ok(value.to_string());
    }
    if !value.ends_with('"') || value.len() < 2 {
        return Err(());
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 1;
    while index < bytes.len() - 1 {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index >= bytes.len() - 1 {
            return Err(());
        }
        match bytes[index] {
            b'a' => decoded.push(7),
            b'b' => decoded.push(8),
            b't' => decoded.push(9),
            b'n' => decoded.push(10),
            b'v' => decoded.push(11),
            b'f' => decoded.push(12),
            b'r' => decoded.push(13),
            b'"' => decoded.push(b'"'),
            b'\\' => decoded.push(b'\\'),
            digit @ b'0'..=b'7' => {
                let mut number = digit - b'0';
                for _ in 0..2 {
                    index += 1;
                    if index >= bytes.len() - 1 || !(b'0'..=b'7').contains(&bytes[index]) {
                        return Err(());
                    }
                    number = number * 8 + bytes[index] - b'0';
                }
                decoded.push(number);
            }
            _ => return Err(()),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| ())
}

pub(crate) fn parse_worktrees(output: &[u8]) -> Result<Vec<Worktree>, CommandError> {
    let text = std::str::from_utf8(output)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid text."))?;
    let mut records = Vec::new();
    let mut current = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            if !current.is_empty() {
                records.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        records.push(current);
    }
    let mut worktrees = Vec::new();
    for record in records {
        if record.iter().any(|line| *line == "bare") {
            continue;
        }
        if !record
            .first()
            .is_some_and(|line| line.starts_with("worktree "))
        {
            return Err(CommandError::new(
                "git_output_malformed",
                "Git returned malformed worktree data.",
            ));
        }
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        let mut detached = false;
        for line in record {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = decode_git_path(value)
                    .ok()
                    .filter(|value| !value.is_empty());
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                if value.is_empty() {
                    return Err(CommandError::new(
                        "git_output_malformed",
                        "Git returned an empty worktree HEAD.",
                    ));
                }
                head = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("branch ") {
                if value.is_empty() {
                    return Err(CommandError::new(
                        "git_output_malformed",
                        "Git returned an empty worktree branch.",
                    ));
                }
                branch = Some(value.to_string());
            } else if line == "detached" {
                detached = true;
            } else if line == "locked"
                || line.starts_with("locked ")
                || line.starts_with("prunable ")
                || line
                    .split_once(' ')
                    .is_some_and(|(key, value)| !key.is_empty() && !value.is_empty())
            {
                continue;
            } else {
                return Err(CommandError::new(
                    "git_output_malformed",
                    "Git returned malformed worktree data.",
                ));
            }
        }
        let path = path.ok_or_else(|| {
            CommandError::new(
                "git_output_malformed",
                "Git returned a worktree without a path.",
            )
        })?;
        let head = head.ok_or_else(|| {
            CommandError::new(
                "git_output_malformed",
                "Git returned a worktree without a HEAD.",
            )
        })?;
        let branch = if detached {
            "detached".into()
        } else {
            branch.ok_or_else(|| {
                CommandError::new(
                    "git_output_malformed",
                    "Git returned a worktree without a branch.",
                )
            })?
        };
        worktrees.push(Worktree { path, branch, head });
    }
    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_records() {
        let output = br#"worktree /tmp/main
HEAD abc123
branch refs/heads/main
locked reason
unknown value

worktree "/tmp/space\040name"
HEAD def456
detached
prunable stale

bare
"#;
        assert_eq!(
            parse_worktrees(output).unwrap(),
            vec![
                Worktree {
                    path: "/tmp/main".into(),
                    branch: "refs/heads/main".into(),
                    head: "abc123".into()
                },
                Worktree {
                    path: "/tmp/space name".into(),
                    branch: "detached".into(),
                    head: "def456".into()
                }
            ]
        );
    }
    #[test]
    fn rejects_incomplete_porcelain() {
        assert!(parse_worktrees(b"worktree /tmp/main\nbranch refs/heads/main\n").is_err());
    }
    #[test]
    fn parses_name_status_records_and_renames() {
        let output = b"A\0new.txt\0M\0changed.txt\0R100\0old.txt\0new.txt\0";
        let files = parse_name_status(output).unwrap();
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[1].status, "M");
        assert_eq!(files[2].path, "new.txt");
        assert!(parse_name_status(b"A\0\xff\0").is_err());
    }
    #[test]
    fn parses_numstat_records_and_binary_rows() {
        let output = b"3\t2\tfile.txt\00\t0\t\0old.txt\0new.txt\0-\t-\timage.png\01\t0\t\0/dev/null\0new-file.txt\0";
        assert_eq!(parse_numstat(output).unwrap(), (4, 2, true));
        assert!(parse_numstat(b"3\t2\t\0old.txt\0").is_err());
    }
    #[test]
    fn parses_untracked_paths() {
        assert_eq!(
            parse_untracked_paths(b"a.txt\0dir/b.txt\0").unwrap(),
            ["a.txt", "dir/b.txt"]
        );
        assert!(parse_untracked_paths(b"a\0\xff\0").is_err());
    }

    #[test]
    fn parses_branch_records_with_edge_fields() {
        let record = |refname: &str, upstream: &str, track: &str| {
            format!(
                "{refname}\u{1f}{}\u{1f}1768176000\u{1f}A U Thor\u{1f}handles, commas, 100% signs and naïve 🌲 subjects\u{1f}{upstream}\u{1f}{track}\u{1e}\n",
                "a".repeat(40)
            )
        };
        let output = format!(
            "{}{}{}",
            record("refs/heads/main", "refs/remotes/origin/main", ""),
            record("refs/heads/feature", "", "[ahead 2, behind 1]"),
            record("refs/heads/gone-upstream", "refs/remotes/origin/gone", "[gone]"),
        );
        let branches = parse_branch_records(output.as_bytes()).unwrap();
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0].ref_name, "refs/heads/main");
        assert_eq!(branches[0].commit_date, 1768176000);
        assert_eq!(branches[0].subject, "handles, commas, 100% signs and naïve 🌲 subjects");
        assert_eq!(branches[0].upstream.as_deref(), Some("refs/remotes/origin/main"));
        assert_eq!(branches[1].upstream, None);
        assert_eq!(branches[1].track, "[ahead 2, behind 1]");
        assert_eq!(branches[2].track, "[gone]");

        assert!(parse_branch_records(b"").unwrap().is_empty());
        // Truncated records (separator bytes inside metadata) are skipped.
        let short_record = format!("refs/heads/main\u{1f}{}\u{1f}0\u{1f}a\u{1f}s\u{1e}\n", "a".repeat(40));
        assert!(parse_branch_records(short_record.as_bytes()).unwrap().is_empty());
        let not_a_branch = format!(
            "refs/tags/v1\u{1f}{}\u{1f}0\u{1f}a\u{1f}s\u{1f}\u{1f}\u{1e}\n",
            "a".repeat(40)
        );
        assert!(parse_branch_records(not_a_branch.as_bytes()).is_err());
        let bad_date = format!(
            "refs/heads/main\u{1f}{}\u{1f}not-a-date\u{1f}a\u{1f}s\u{1f}\u{1f}\u{1e}\n",
            "a".repeat(40)
        );
        assert!(parse_branch_records(bad_date.as_bytes()).is_err());
        // Separator bytes inside the subject reassemble into one record.
        let split_subject = format!(
            "refs/heads/main\u{1f}{}\u{1f}1768176000\u{1f}A\u{1f}broken\u{1f}subject\u{1f}refs/remotes/origin/main\u{1f}\u{1e}\n",
            "a".repeat(40)
        );
        let branches = parse_branch_records(split_subject.as_bytes()).unwrap();
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].subject, "broken\u{1f}subject");
        assert_eq!(
            branches[0].upstream.as_deref(),
            Some("refs/remotes/origin/main")
        );
    }

    #[test]
    fn parses_commit_log_records_with_edge_subjects() {
        let record = |sha: &str, subject: &str, refs: &str, parents: &str| {
            format!("{sha}\u{1f}{subject}\u{1f}A U Thor\u{1f}2026-01-02T03:04:05+00:00\u{1f}{refs}\u{1f}{parents}\u{1e}\n")
        };
        let output = format!(
            "{}{}{}",
            record(
                &"a".repeat(40),
                "handles, commas, 100% signs and naïve 🌲 subjects",
                "refs/heads/main, refs/remotes/origin/main",
                "",
            ),
            record(&"b".repeat(40), "merge commit", "", "1111111 2222222 3333333"),
            record(&"c".repeat(40), "tagged root", "refs/tags/v1", ""),
        );
        let commits = parse_commits(output.as_bytes()).unwrap();
        assert_eq!(commits.len(), 3);
        assert_eq!(
            commits[0].subject,
            "handles, commas, 100% signs and naïve 🌲 subjects"
        );
        assert_eq!(
            commits[0].refs,
            ["refs/heads/main", "refs/remotes/origin/main"]
        );
        assert!(commits[0].parents.is_empty());
        assert!(!commits[0].default_base_ancestor);
        assert_eq!(commits[1].parents, ["1111111", "2222222", "3333333"]);
        assert!(commits[1].refs.is_empty());
        assert_eq!(commits[2].refs, ["refs/tags/v1"]);
        assert!(commits[2].parents.is_empty());

        assert!(parse_commits(b"").unwrap().is_empty());
        assert!(parse_commits(b"\n").unwrap().is_empty());
        let short_record = format!("{}\u{1f}subject\u{1e}\n", "b".repeat(40));
        assert!(parse_commits(short_record.as_bytes()).is_err());
        let extra_record = format!(
            "{}\u{1f}sub\u{1f}ject\u{1f}author\u{1f}date\u{1f}refs\u{1f}parents\u{1e}\n",
            "c".repeat(40)
        );
        assert!(parse_commits(extra_record.as_bytes()).is_err());
        let no_fields = b"garbage-without-separators";
        assert!(parse_commits(no_fields).is_err());
    }
}
