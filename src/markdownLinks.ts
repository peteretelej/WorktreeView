import { parseCodeRef, type CodeRef } from "./comments.ts";

// Any absolute scheme (https, mailto, irc, ...) hands off to the OS; only
// scheme-less repo-relative links can resolve in-app.
const HREF_SCHEME = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;
const HREF_LINE_FRAGMENT = /^([^#]+)#L(\d+)(?:-L(\d+))?$/;

export function isExternalHref(href: string): boolean {
  const trimmed = href.trim();
  return trimmed.length > 0 && HREF_SCHEME.test(trimmed);
}

// Markdown links to repo files jump in-app like the backticked `path:line`
// spans: `src/auth.rs`, `src/auth.rs:18`, `src/auth.rs#L12-L20`. Schemes,
// absolute paths, traversal, and bare fragments never resolve.
export function parseHrefCodeRef(href: string): CodeRef | null {
  const trimmed = href.trim();
  if (!trimmed || HREF_SCHEME.test(trimmed) || trimmed.startsWith("#") || trimmed.startsWith("/") || trimmed.startsWith("..")) return null;
  const fragment = HREF_LINE_FRAGMENT.exec(trimmed);
  if (fragment) {
    const path = fragment[1];
    if (!/[./]/.test(path) || path.endsWith(".")) return null;
    return { path, start: parseInt(fragment[2], 10), end: fragment[3] ? parseInt(fragment[3], 10) : null, sha: null };
  }
  const colon = parseCodeRef(trimmed);
  if (colon) return colon;
  // A bare repo path links the file's first line.
  if (/[./]/.test(trimmed) && !trimmed.endsWith(".")) return { path: trimmed, start: 1, end: null, sha: null };
  return null;
}
