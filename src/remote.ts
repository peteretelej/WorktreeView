// The one frontend dispatch seam: a `call` shaped like Tauri's invoke that
// routes a command either to the local backend (local source) or to a
// configured server over its command API (server source, bearer token from
// the local store's connection rows). Carries no Git or filesystem
// semantics; source resolution consults the local `server_projects` rows
// and a registered local repo always wins over a server mapping.
import { invoke } from "@tauri-apps/api/core";
import type { PortalActivityEvent, PortalReviewRow, PortalSearchMatches, PortalThreadGroup } from "./portal.ts";
import type { AttentionQueue } from "./requests.ts";
import { errorMessage, type CommandError } from "./format.ts";

// Mirrors the Rust `ServerProject` row.
export type ServerProjectRow = { id: number; connection_id: number; repo_path: string; pinned: boolean; created_at: number };
// Mirrors the Rust `ServerConnection` row with its projects nested.
export type ServerConnectionRow = { id: number; url: string; label: string | null; token: string; created_at: number; projects: ServerProjectRow[] };

// Where a command runs: the local backend, or one server connection.
export type RemoteSource = { kind: "local" } | { kind: "server"; connectionId: number; url: string; token: string };
export type SourceResolver = (repoPath: string) => RemoteSource;

const LOCAL: RemoteSource = { kind: "local" };

function serverSource(connection: ServerConnectionRow): RemoteSource {
  return { kind: "server", connectionId: connection.id, url: connection.url, token: connection.token };
}

// Sends one command to its source. Server answers serialize exactly as the
// IPC layer would; failures surface as `{code, message}` like a refused
// invoke, whatever shape the transport answered with.
export async function call<T>(command: string, args: Record<string, unknown> | undefined, source: RemoteSource): Promise<T> {
  if (source.kind === "local") return invoke<T>(command, args ?? {});
  const response = await fetch(`${source.url}/api/${command}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${source.token}` },
    body: JSON.stringify(args ?? {}),
  });
  if (!response.ok) throw remoteErrorFrom(response.status, await response.text());
  // The one bytes-shaped route: raw file content, not JSON.
  if (command === "read_review_file_bytes") return response.arrayBuffer() as T;
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

// A registered local repo wins over a server mapping; an unknown path
// falls back to local so first-open behavior never changes.
export function resolveSource(repoPath: string, localPaths: Iterable<string>, connections: ServerConnectionRow[]): RemoteSource {
  for (const path of localPaths) if (path === repoPath) return LOCAL;
  for (const connection of connections) {
    for (const project of connection.projects) {
      if (project.repo_path === repoPath) return serverSource(connection);
    }
  }
  return LOCAL;
}

export function sourceForConnection(connectionId: number | null | undefined, connections: ServerConnectionRow[]): RemoteSource {
  if (connectionId === null || connectionId === undefined) return LOCAL;
  const connection = connections.find((candidate) => candidate.id === connectionId);
  return connection ? serverSource(connection) : LOCAL;
}

// Every backend the app renders from: local first, then one source per
// connection. Global listings fan out over this list and merge.
export function connectionSources(connections: ServerConnectionRow[]): RemoteSource[] {
  return [LOCAL, ...connections.map(serverSource)];
}

// The tree group's label: the connection's label, else its host.
export function connectionLabel(connection: Pick<ServerConnectionRow, "url" | "label">): string {
  const label = connection.label?.trim();
  if (label) return label;
  try {
    const host = new URL(connection.url).host;
    if (host) return host;
  } catch { /* fall through to the raw URL */ }
  return connection.url;
}

// Display name for a host path: the last segment, Windows or POSIX.
export function repoNameFromPath(repoPath: string): string {
  const segments = repoPath.split(/[\\/]/).filter(Boolean);
  return segments[segments.length - 1] ?? repoPath;
}

// Trailing slashes are presentation; the scheme and host are the contract.
export function normalizeServerUrl(url: string): string | null {
  const trimmed = url.trim().replace(/\/+$/, "");
  if (!trimmed) return null;
  try {
    const parsed = new URL(trimmed);
    if ((parsed.protocol !== "http:" && parsed.protocol !== "https:") || !parsed.hostname) return null;
  } catch {
    return null;
  }
  return trimmed;
}

// Server refusals share the IPC `{code, message}` shape, but a refused or
// oversized body can arrive as plain text or in the JSON-RPC error shape
// (numeric code); every shape lands here as a displayable command error.
export function remoteErrorFrom(status: number, body: string): CommandError {
  try {
    const parsed: unknown = JSON.parse(body);
    if (parsed && typeof parsed === "object" && "message" in parsed) {
      const shaped = parsed as { code?: unknown; message?: unknown };
      const code = typeof shaped.code === "string" && shaped.code !== ""
        ? shaped.code
        : typeof shaped.code === "number"
          ? `server_error_${shaped.code}`
          : "server_error";
      return { code, message: typeof shaped.message === "string" && shaped.message !== "" ? shaped.message : `The server returned HTTP ${status}.` };
    }
  } catch { /* not JSON */ }
  return { code: "server_error", message: `The server returned HTTP ${status}.` };
}

// The save-connection probe: one authenticated list_repos against the
// target server, proving the URL answers and the token works before a
// local row is written. Throws the server's error shape on refusal.
export async function probeConnection(url: string, token: string): Promise<void> {
  const normalized = normalizeServerUrl(url);
  if (!normalized) {
    throw { code: "invalid_connection", message: "Enter an http:// or https:// URL with a host name." } satisfies CommandError;
  }
  await call("list_repos", undefined, { kind: "server", connectionId: 0, url: normalized, token });
}

// ===== Merged global listings =====
// Global Pulse reads fan out over every source and merge client-side.
// Rows keep the repo_path they carry, so project filters, labels, and
// review opens work unchanged; rows that open an id-keyed detail carry the
// owning connection's id. The local page is always the first element of
// each pages array.

export function mergeAttention(queues: AttentionQueue[]): AttentionQueue {
  return { repos: queues.flatMap((queue) => queue.repos) };
}

export function mergePortalReviews(pages: PortalReviewRow[][]): PortalReviewRow[] {
  return pages.flat().sort((left, right) => right.last_activity_at - left.last_activity_at);
}

export function mergePortalThreadGroups(pages: PortalThreadGroup[][]): PortalThreadGroup[] {
  return pages.flat();
}

export function mergeSearchMatches(matches: PortalSearchMatches[]): PortalSearchMatches {
  return {
    comments: matches.flatMap((match) => match.comments),
    requests: matches.flatMap((match) => match.requests),
    commits: matches.flatMap((match) => match.commits),
  };
}

// The merged activity feed keeps each source's newest-first order under a
// created_at sort; the seen watermark stays the local backend's (the seen
// cursor is client-local and the mark command is local-only), so only
// untagged rows can read as new.
export function mergePortalActivity(pages: Array<{ events: PortalActivityEvent[]; seen_id: number }>): { events: PortalActivityEvent[]; seen_id: number } {
  return {
    events: pages.flatMap((page) => page.events).sort((left, right) => right.created_at - left.created_at),
    seen_id: pages[0]?.seen_id ?? 0,
  };
}

// Divider position for a merged feed: the first event newer than the
// local watermark. Connection-tagged rows never count as new, so for an
// all-local feed this reduces to the store's own divider.
export function activityDividerIndexForSources(events: PortalActivityEvent[], seenId: number): number {
  return events.findIndex((event) => (event.connection_id ?? null) === null && event.id > seenId);
}

// ===== Server event stream =====

// One parsed server-sent-event frame: the push event's name and its JSON
// payload. Keepalive comments and partial chunks never surface here.
export type SseFrame = { event: string; data: unknown };

// Splits a stream buffer into complete frames, returning the trailing
// partial frame for the next chunk. A frame ends at a blank line (LF or
// CRLF); keepalive comments and unknown fields carry no payload, `data:`
// lines join so a split payload survives, and a malformed frame is
// skipped rather than blocking the frames behind it.
export function parseSseFrames(buffer: string): { frames: SseFrame[]; rest: string } {
  const frames: SseFrame[] = [];
  let rest = buffer;
  for (;;) {
    const end = /\r?\n\r?\n/.exec(rest);
    if (!end) break;
    let event = "";
    const data: string[] = [];
    for (const line of rest.slice(0, end.index).split("\n")) {
      const field = line.replace(/\r$/, "");
      if (field.startsWith("event:")) event = field.slice("event:".length).trim();
      else if (field.startsWith("data:")) data.push(field.slice("data:".length).replace(/^ /, ""));
    }
    rest = rest.slice(end.index + end[0].length);
    if (!event || data.length === 0) continue;
    try {
      frames.push({ event, data: JSON.parse(data.join("\n")) });
    } catch { /* a frame we cannot read is dropped, not fatal */ }
  }
  return { frames, rest };
}

// The reconnect delays: first retry is quick, then backs off to a ceiling
// (the documented reconnect stance; no polling fallback is built).
const SSE_RETRY_BASE_MS = 1000;
const SSE_RETRY_MAX_MS = 60000;

export type EventStreamHandlers = {
  onEvent(frame: SseFrame): void;
  // A failed establishment surfaces once per failed stretch; backoff
  // retries stay quiet until a stream establishes again.
  onUnestablished(error: CommandError): void;
};

// One server's live-event stream: a fetch-stream reader carrying the
// connection's bearer header (EventSource cannot set headers), held open
// with exponential-backoff reconnects for as long as the app keeps the
// closer. Closing aborts the in-flight read and any pending retry.
export function connectEventStream(url: string, token: string, handlers: EventStreamHandlers): () => void {
  const controller = new AbortController();
  let closed = false;
  let retryTimer: number | undefined;
  let delay = SSE_RETRY_BASE_MS;
  let surfaced = false;

  async function run() {
    while (!closed) {
      try {
        const response = await fetch(`${url}/events`, { headers: { Authorization: `Bearer ${token}` }, signal: controller.signal });
        if (!response.ok) throw remoteErrorFrom(response.status, await response.text());
        // No body means this webview cannot stream fetch responses at all:
        // surface once, then keep the (pointless) retries quiet.
        if (!response.body) throw { code: "server_error", message: "The server event stream is not readable in this webview." } satisfies CommandError;
        surfaced = false;
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          const parsed = parseSseFrames(buffer);
          buffer = parsed.rest;
          for (const frame of parsed.frames) handlers.onEvent(frame);
        }
      } catch (error) {
        if (closed || controller.signal.aborted) return;
        if (!surfaced) {
          surfaced = true;
          handlers.onUnestablished(typeof error === "object" && error !== null && "code" in error
            ? error as CommandError
            : { code: "server_error", message: `The server event stream could not be reached: ${errorMessage(error)}` });
        }
      }
      if (closed) return;
      retryTimer = window.setTimeout(() => { retryTimer = undefined; void run(); }, delay);
      delay = Math.min(delay * 2, SSE_RETRY_MAX_MS);
    }
  }
  void run();
  return () => {
    closed = true;
    if (retryTimer !== undefined) window.clearTimeout(retryTimer);
    controller.abort();
  };
}
