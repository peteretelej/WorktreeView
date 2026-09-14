// Shared formatting and error helpers extracted from App.tsx.


export type CommandError = { code: string; message: string };
export function errorMessage(error: unknown) {
  if (typeof error === "object" && error !== null && "code" in error) {
    const commandError = error as CommandError;
    if (commandError.code === "not_git_repository" || commandError.code === "invalid_path") return commandError.message;
    if (commandError.code === "persistence") return "Repository storage is unavailable.";
    if (commandError.code === "git_timeout") return commandError.message;
    if (commandError.code === "git_output_too_large") return "Git returned too much worktree data.";
    if (commandError.code === "git_output_malformed") return "Git returned malformed worktree data.";
    if (commandError.code === "git_filter_unsupported") return "This review cannot run because Git conversion filters apply to files in this review.";
    // git_execution's message carries Git's own stderr; surfacing it beats a
    // generic line, since the log file is the only other witness.
    if (commandError.code === "git_execution") return commandError.message || "Git could not inspect this repository.";
    if (commandError.code === "partial_clone_content") return commandError.message;
    if (commandError.code === "content_unavailable") return "This surface's content is no longer available in the repository.";
    if (commandError.code === "scope_requires_worktree") return "All changes scope requires the worktree's checked-out state.";
    if (commandError.code === "unresolvable_ref") return commandError.message;
    if (commandError.code === "invalid_submission") return commandError.message;
    if (commandError.code === "unknown_review_target") return commandError.message;
  }
  if (typeof error === "object" && error !== null && "message" in error) return String(error.message);
  return "The repository operation failed.";
}

export function errorCodeOf(error: unknown) {
  return typeof error === "object" && error !== null && "code" in error ? String((error as CommandError).code) : "";
}

// Mirrors the Rust u32::MAX sentinel for status counts that exceeded the
// output bound.
const STATUS_COUNT_OVERFLOW = 4294967295;

export function changesLabel(changes: number | null | undefined) {
  if (changes === null || changes === undefined) return "";
  if (changes === 0) return "Clean";
  if (changes === STATUS_COUNT_OVERFLOW) return "9999+ changed";
  return `${changes} changed`;
}

// Shortens a refname for display. Git log decorations (%D) ride with
// prefixes, so "tag: refs/tags/v1" and "HEAD -> refs/heads/main" strip to
// the refname before the refs/ hierarchy comes off.
export function shortToken(token: string) {
  const refname = token.includes(" -> ") ? token.slice(token.indexOf(" -> ") + 4) : token.replace(/^tag:\s*/, "");
  if (refname.startsWith("refs/heads/")) return refname.slice("refs/heads/".length);
  if (refname.startsWith("refs/remotes/")) return refname.slice("refs/remotes/".length);
  if (refname.startsWith("refs/tags/")) return refname.slice("refs/tags/".length);
  return /^[0-9a-f]{40}$/i.test(refname) ? refname.slice(0, 7) : refname;
}

// Commit rows and meta lines only have room for a given name's first word.
export function shortAuthor(author: string) {
  return author.split(/\s+/)[0] || author;
}

// Sidebar history rows abbreviate the author to initials ("PE"); the full
// name rides in the title attribute.
export function authorInitials(author: string) {
  const words = author.split(/\s+/).filter(Boolean);
  if (words.length === 0) return "";
  const last = words.length > 1 ? words[words.length - 1].charAt(0) : "";
  return `${words[0].charAt(0)}${last}`.toUpperCase();
}

// Ultra-compact commit age for history rows: "now", "5m", "3h", "1d", "2w",
// then a short calendar date once weeks stop being precise enough.
export function compactAge(dateIso: string) {
  const seconds = Math.floor((Date.now() - Date.parse(dateIso)) / 1000);
  if (!Number.isFinite(seconds)) return "";
  if (seconds < 60) return "now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  const weeks = Math.floor(days / 7);
  if (weeks < 5) return `${weeks}w`;
  return new Date(dateIso).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

// Branches untouched for a week read as stale; agent worktrees often are.
export const STALE_AFTER_SECONDS = 7 * 24 * 60 * 60;

export function relativeTime(unixSeconds: number) {
  const seconds = Math.floor(Date.now() / 1000) - unixSeconds;
  if (seconds < 45) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(unixSeconds * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

// host + path display form of a remote URL; copying always uses the full URL.
export function originSlug(url: string) {
  return url.replace(/\.git$/i, "").replace(/^(https?|ssh|git):\/\//i, "").replace(/^[^/@]+@/, "").replace(":", "/");
}

