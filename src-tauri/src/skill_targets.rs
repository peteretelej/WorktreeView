//! Installing the bundled agent skill into agents' skill directories
//! under the user profile. Targets are the known skills folders of the
//! Agent Skills ecosystem; the app writes only the `worktreeview` folder
//! inside them, and only on the user's explicit Install or Update action.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::home::SKILL_BUNDLE;

/// Known agent skills folders under the user profile, each with the label
/// the Settings row shows. `.agents/skills` is the ecosystem's shared
/// location (the skills CLI's target, read by Codex CLI and friends);
/// `.claude/skills` is Claude Code's own folder. New clients join this
/// list as their conventions settle.
const KNOWN_SKILLS_DIRS: &[(&str, &str)] = &[
    (".agents/skills", "Shared agent skills"),
    (".claude/skills", "Claude Code"),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkillTargetStatus {
    NotInstalled,
    UpToDate,
    Differs,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SkillTarget {
    pub path: String,
    pub label: String,
    pub status: SkillTargetStatus,
}

fn target_dir(home_root: &Path, skills_dir: &str) -> PathBuf {
    home_root.join(skills_dir).join("worktreeview")
}

/// The known targets whose skills directory actually exists: an agent
/// app that never created its folder is not offered a target, so the
/// app never invents directories for clients the user does not run.
pub(crate) fn detect(home_root: &Path) -> Vec<SkillTarget> {
    KNOWN_SKILLS_DIRS
        .iter()
        .filter(|(dir, _)| home_root.join(dir).is_dir())
        .map(|(dir, label)| {
            let path = target_dir(home_root, dir);
            SkillTarget {
                path: path.to_string_lossy().into_owned(),
                label: label.to_string(),
                status: status_of(&path),
            }
        })
        .collect()
}

/// Pure content comparison against the bundled files: every bundled file
/// present and identical means current, a missing or changed one means
/// the folder needs a copy, and extra files never count (installs remove
/// nothing). Foreign installs (`npx skills add`, manual copies) classify
/// with the same rule, no install tracking involved.
pub(crate) fn status_of(target: &Path) -> SkillTargetStatus {
    if !target.is_dir() {
        return SkillTargetStatus::NotInstalled;
    }
    for (name, contents) in SKILL_BUNDLE {
        let identical = std::fs::read_to_string(target.join(name))
            .map(|existing| existing == *contents)
            .unwrap_or(false);
        if !identical {
            return SkillTargetStatus::Differs;
        }
    }
    SkillTargetStatus::UpToDate
}

/// Copy the bundled skill into the target, creating the folder when
/// needed. Mirrors the home publish: file-for-file, strays untouched.
pub(crate) fn install(target: &Path) -> std::io::Result<SkillTargetStatus> {
    std::fs::create_dir_all(target)?;
    for (name, contents) in SKILL_BUNDLE {
        std::fs::write(target.join(name), contents)?;
    }
    Ok(SkillTargetStatus::UpToDate)
}

#[cfg(test)]
mod tests {
    use super::{detect, install, status_of, SkillTargetStatus};

    #[test]
    fn detection_lists_only_existing_skills_directories() {
        let home = crate::testutil::test_path("skill-target-detect");
        let claude = home.join(".claude").join("skills");
        std::fs::create_dir_all(&claude).unwrap();

        let targets = detect(&home);

        assert_eq!(targets.len(), 1, "a missing skills directory is not a target");
        assert_eq!(targets[0].label, "Claude Code");
        assert!(targets[0].path.ends_with("worktreeview"));
        assert_eq!(targets[0].status, SkillTargetStatus::NotInstalled);
    }

    #[test]
    fn install_writes_the_bundle_and_status_follows_content() {
        let home = crate::testutil::test_path("skill-target-install");
        let skills = home.join(".agents").join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let target = skills.join("worktreeview");

        assert_eq!(status_of(&target), SkillTargetStatus::NotInstalled);
        install(&target).unwrap();
        assert_eq!(status_of(&target), SkillTargetStatus::UpToDate);

        // A stale or hand-edited file and a missing one both read as
        // differing; updating rewrites the bundle and leaves strays.
        std::fs::write(target.join("SKILL.md"), "stale").unwrap();
        assert_eq!(status_of(&target), SkillTargetStatus::Differs);
        std::fs::write(target.join("stray.txt"), "kept").unwrap();
        install(&target).unwrap();
        assert_eq!(status_of(&target), SkillTargetStatus::UpToDate);
        assert_eq!(std::fs::read_to_string(target.join("stray.txt")).unwrap(), "kept");
    }

    #[test]
    fn missing_bundle_file_reads_as_differing() {
        let home = crate::testutil::test_path("skill-target-missing-file");
        let target = home.join(".agents").join("skills").join("worktreeview");
        install(&target).unwrap();
        std::fs::remove_file(target.join("performing-review.md")).unwrap();

        assert_eq!(status_of(&target), SkillTargetStatus::Differs);
    }
}
