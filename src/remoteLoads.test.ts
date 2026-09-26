import { test } from "node:test";
import assert from "node:assert/strict";
import { unwrapLoad, type RemoteLoad } from "./remoteLoads.ts";

type Inventory = { default_branch: string };
type Row = { path: string };

function remoteLoad<T>(overrides: Partial<RemoteLoad<T>>): RemoteLoad<T> {
  return { state: "live", last_success_age_ms: 0, message: null, data: null, ...overrides };
}

test("local loads pass through untouched", () => {
  const worktrees = [{ path: "/wt/demo", branch: "refs/heads/demo", head: "sha" }];
  const load = unwrapLoad(worktrees);
  assert.equal(load.remote, false);
  assert.equal(load.state, null);
  assert.equal(load.message, null);
  assert.equal(load.data, worktrees);

  const inventory: Inventory = { default_branch: "refs/heads/main" };
  const objectLoad = unwrapLoad(inventory);
  assert.equal(objectLoad.remote, false);
  assert.equal(objectLoad.data, inventory);
});

test("wrapped remote loads keep their health state and message", () => {
  const payload = remoteLoad<Row[]>({ state: "live", data: [{ path: "/srv/repo" }] });
  const load = unwrapLoad(payload);
  assert.deepEqual(load, { remote: true, state: "live", message: null, data: [{ path: "/srv/repo" }] });

  const offline = unwrapLoad(remoteLoad<Row[]>({ state: "offline", message: "host unreachable", data: null }));
  assert.equal(offline.remote, true);
  assert.equal(offline.state, "offline");
  assert.equal(offline.message, "host unreachable");
  assert.equal(offline.data, null);

  const stale = unwrapLoad(remoteLoad<Row[]>({ state: "stale", last_success_age_ms: 40000 }));
  assert.equal(stale.remote, true);
  assert.equal(stale.state, "stale");
  assert.equal(stale.data, null);
});

test("objects that merely carry a state field stay local", () => {
  const odd = { state: "archived", rows: [] } as unknown as Row[];
  const load = unwrapLoad(odd);
  assert.equal(load.remote, false);
  assert.equal(load.data, odd);
});
