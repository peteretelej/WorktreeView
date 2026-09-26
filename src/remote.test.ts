import { test } from "node:test";
import assert from "node:assert/strict";
import {
  activityDividerIndexForSources,
  call,
  connectionLabel,
  connectionSources,
  mergeAttention,
  mergePortalActivity,
  mergePortalReviews,
  normalizeServerUrl,
  probeConnection,
  remoteErrorFrom,
  repoNameFromPath,
  resolveSource,
  sourceForConnection,
  type ServerConnectionRow,
} from "./remote.ts";
import type { AttentionQueue } from "./requests.ts";

const lab: ServerConnectionRow = {
  id: 2,
  url: "http://lab.example.com:9887",
  label: null,
  token: "tok-lab",
  created_at: 5,
  projects: [
    { id: 20, connection_id: 2, repo_path: "/srv/demo", pinned: false, created_at: 5 },
    { id: 21, connection_id: 2, repo_path: "/srv/other", pinned: true, created_at: 6 },
  ],
};
const home: ServerConnectionRow = {
  id: 3,
  url: "http://home:9887/",
  label: "home",
  token: "tok-home",
  created_at: 4,
  projects: [{ id: 30, connection_id: 3, repo_path: "/srv/demo", pinned: false, created_at: 7 }],
};
const connections = [lab, home];

test("resolveSource routes local paths first and falls back to local", () => {
  // A registered local repo wins even when a server maps the same path.
  assert.deepEqual(resolveSource("/local/demo", ["/local/demo", "/srv/demo"], connections), { kind: "local" });
  // Server-backed paths resolve to their owning connection.
  assert.deepEqual(resolveSource("/srv/other", ["/local/demo"], connections), {
    kind: "server",
    connectionId: 2,
    url: "http://lab.example.com:9887",
    token: "tok-lab",
  });
  // Unknown paths fall back to local so first-open behavior never changes.
  assert.deepEqual(resolveSource("/elsewhere", ["/local/demo"], connections), { kind: "local" });
  assert.deepEqual(resolveSource("/nowhere", [], connections), { kind: "local" });
});

test("sourceForConnection maps ids to sources and null to local", () => {
  assert.deepEqual(sourceForConnection(null, connections), { kind: "local" });
  assert.deepEqual(sourceForConnection(undefined, connections), { kind: "local" });
  const homeSource = sourceForConnection(3, connections);
  assert.equal(homeSource.kind, "server");
  if (homeSource.kind === "server") {
    assert.equal(homeSource.url, "http://home:9887/");
  }
  // A connection deleted since the row was rendered falls back to local.
  assert.deepEqual(sourceForConnection(99, connections), { kind: "local" });
});

test("connectionSources lists local first, then one per connection", () => {
  const sources = connectionSources(connections);
  assert.equal(sources.length, 3);
  assert.deepEqual(sources[0], { kind: "local" });
  assert.equal(sources[1].kind, "server");
  assert.equal(sources[2].kind, "server");
});

test("connectionLabel prefers the label and falls back to the host", () => {
  assert.equal(connectionLabel(lab), "lab.example.com:9887");
  assert.equal(connectionLabel(home), "home");
  assert.equal(connectionLabel({ url: "not a url", label: null }), "not a url");
});

test("repoNameFromPath reads the last path segment", () => {
  assert.equal(repoNameFromPath("/srv/demo"), "demo");
  assert.equal(repoNameFromPath("C:\\repos\\demo"), "demo");
  assert.equal(repoNameFromPath("/"), "/");
});

test("normalizeServerUrl trims trailing slashes and refuses non-http targets", () => {
  assert.equal(normalizeServerUrl(" http://host:9887/ "), "http://host:9887");
  assert.equal(normalizeServerUrl("https://lab.example.com"), "https://lab.example.com");
  assert.equal(normalizeServerUrl("ftp://host"), null);
  assert.equal(normalizeServerUrl("host:9887"), null);
  assert.equal(normalizeServerUrl("http://"), null);
  assert.equal(normalizeServerUrl("   "), null);
});

// The backend stores connection URLs without trailing slashes; the probe
// strips them from raw input before saving. Either way the joined fetch
// target keeps a single `/api/` segment, which the command route requires.
test("call and the probe hit /api/<command> with no doubled slash", async () => {
  const urls: string[] = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input: RequestInfo | URL) => {
    urls.push(String(input));
    return { ok: true, status: 200, text: async () => "{}" } as Response;
  };
  try {
    // The save-time probe normalizes the raw input's trailing slash.
    await probeConnection(" http://host:9887/ ", "tok-1");
    // A stored row reaches the same single-segment path.
    await call("list_repos", undefined, { kind: "server", connectionId: 2, url: "http://host:9887", token: "tok-1" });
  } finally {
    globalThis.fetch = originalFetch;
  }
  assert.deepEqual(urls, ["http://host:9887/api/list_repos", "http://host:9887/api/list_repos"]);
});

test("remoteErrorFrom accepts the IPC shape and tolerates other shapes", () => {
  assert.deepEqual(remoteErrorFrom(400, '{"code":"invalid_arguments","message":"bad body"}'), {
    code: "invalid_arguments",
    message: "bad body",
  });
  // Payload-too-large refusals arrive in the JSON-RPC error shape.
  assert.deepEqual(remoteErrorFrom(413, '{"code":-32600,"message":"too large"}'), {
    code: "server_error_-32600",
    message: "too large",
  });
  assert.deepEqual(remoteErrorFrom(401, "not json"), {
    code: "server_error",
    message: "The server returned HTTP 401.",
  });
});

test("merged activity feed keeps the local watermark and skips tagged rows for the divider", () => {
  const event = (id: number, created_at: number) => ({
    id,
    repo_path: "/srv/demo",
    kind: "comment_posted",
    base_sha: null,
    target_key: null,
    target_kind: null,
    request_id: null,
    comment_id: null,
    actor_kind: "human" as const,
    actor_name: "you",
    summary: "s",
    created_at,
  });
  const local = { events: [event(9, 50), event(4, 10)], seen_id: 4 };
  const remote = { events: [{ ...event(100, 40), connection_id: 2 }], seen_id: 0 };
  const merged = mergePortalActivity([local, remote]);
  assert.equal(merged.seen_id, 4);
  // created_at order: 50 (local), 40 (remote), 10 (local).
  assert.deepEqual(merged.events.map((item) => item.created_at), [50, 40, 10]);
  // The remote event (id 100, created 40) never counts as new: with the
  // local watermark at 9 the divider is absent even though the untagged
  // scan would have matched the remote id.
  assert.equal(activityDividerIndexForSources(merged.events, 9), -1);
  // Everything newer than the watermark reads as new from the top.
  assert.equal(activityDividerIndexForSources(merged.events, 4), 0);
});

test("merged listings concatenate sources", () => {
  const queue: AttentionQueue = { repos: [{ repo_path: "/local/demo", repo_name: "demo", rows: [] }] };
  const remoteQueue: AttentionQueue = { repos: [{ repo_path: "/srv/demo", repo_name: "demo", rows: [] }] };
  assert.equal(mergeAttention([queue, remoteQueue]).repos.length, 2);
  const rows = [
    [{ repo_path: "/a", last_activity_at: 1 }],
    [{ repo_path: "/b", last_activity_at: 9 }],
  ] as unknown as Parameters<typeof mergePortalReviews>[0];
  assert.deepEqual(mergePortalReviews(rows).map((row) => row.repo_path), ["/b", "/a"]);
});
