mod exec;
mod filters;
mod parse;
mod validate;

pub(crate) use exec::{
    acceptable_diff_exit, fetch_remote_branch, fetch_remotes, git_args, git_execution_error,
    run_git, run_git_with_stdin, stdin_git_command, MAX_OUTPUT,
};
#[cfg(test)]
pub(crate) use exec::{read_bounded, spawn_counted};
pub(crate) use filters::{
    configured_filter_names, filter_override_args, reject_applicable_filters,
};
pub(crate) use parse::{
    parse_branch_records, parse_commits, parse_name_status, parse_numstat, parse_status_count,
    parse_untracked_paths, parse_worktrees, BranchRecord, BranchSummary, ChangedFile, CommitInfo,
    CommitPage, Worktree,
};
pub(crate) use validate::{
    effective_head_ref, ensure_work_tree, partial_clone_failure, primary_branch,
    remote_branch_of_tracking_ref, resolve_empty_tree, resolve_ref, validate_fetch_name,
    validate_file, validate_ref, validate_scope_combination, validate_untracked_combination,
};
