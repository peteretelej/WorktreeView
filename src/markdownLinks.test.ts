import assert from "node:assert/strict";
import test from "node:test";
import { isExternalHref, parseHrefCodeRef } from "./markdownLinks.ts";

test("isExternalHref classifies scheme links as external", () => {
  assert.equal(isExternalHref("https://github.com/git/git"), true);
  assert.equal(isExternalHref("http://example.com"), true);
  assert.equal(isExternalHref("mailto:someone@example.com"), true);
  assert.equal(isExternalHref("irc://chat.example/#room"), true);
});

test("isExternalHref keeps repo-relative and empty hrefs internal", () => {
  assert.equal(isExternalHref("src/auth.rs"), false);
  assert.equal(isExternalHref("./src/auth.rs"), false);
  assert.equal(isExternalHref("#L12"), false);
  assert.equal(isExternalHref(""), false);
  assert.equal(isExternalHref("  "), false);
});

test("parseHrefCodeRef resolves bare repo paths to the first line", () => {
  assert.deepEqual(parseHrefCodeRef("src/auth.rs"), { path: "src/auth.rs", start: 1, end: null, sha: null });
  assert.deepEqual(parseHrefCodeRef("./README.md"), { path: "./README.md", start: 1, end: null, sha: null });
});

test("parseHrefCodeRef resolves line and range forms", () => {
  assert.deepEqual(parseHrefCodeRef("src/auth.rs:18"), { path: "src/auth.rs", start: 18, end: null, sha: null });
  assert.deepEqual(parseHrefCodeRef("src/auth.rs:18-25"), { path: "src/auth.rs", start: 18, end: 25, sha: null });
  assert.deepEqual(parseHrefCodeRef("src/auth.rs#L12"), { path: "src/auth.rs", start: 12, end: null, sha: null });
  assert.deepEqual(parseHrefCodeRef("src/auth.rs#L12-L20"), { path: "src/auth.rs", start: 12, end: 20, sha: null });
  assert.deepEqual(parseHrefCodeRef("src/auth.rs:18@f0c3d21"), { path: "src/auth.rs", start: 18, end: null, sha: "f0c3d21" });
});

test("parseHrefCodeRef rejects schemes, absolute paths, traversal, and junk", () => {
  assert.equal(parseHrefCodeRef("https://github.com/git/git"), null);
  assert.equal(parseHrefCodeRef("mailto:someone@example.com"), null);
  assert.equal(parseHrefCodeRef("C:\\repo\\auth.rs"), null);
  assert.equal(parseHrefCodeRef("/etc/passwd"), null);
  assert.equal(parseHrefCodeRef("../sibling/README.md"), null);
  assert.equal(parseHrefCodeRef("#L12"), null);
  assert.equal(parseHrefCodeRef("justword"), null);
  assert.equal(parseHrefCodeRef(""), null);
});
