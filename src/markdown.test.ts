import { test } from "node:test";
import assert from "node:assert/strict";
import { markdownSchema } from "./markdown-schema.ts";

const tagNames = markdownSchema.tagNames ?? [];
const allAttributes = Object.values(markdownSchema.attributes ?? {}).flat();
const allProtocols = Object.values(markdownSchema.protocols ?? {}).flat();

test("the sanitize schema never allows script or iframe elements", () => {
  assert.equal(tagNames.includes("script"), false);
  assert.equal(tagNames.includes("iframe"), false);
});

test("the sanitize schema never allows event handler attributes", () => {
  const handlers = allAttributes.filter((attribute) => typeof attribute === "string" && attribute.toLowerCase().startsWith("on"));
  assert.deepEqual(handlers, []);
});

test("the sanitize schema never allows javascript URLs", () => {
  assert.equal(allProtocols.includes("javascript"), false);
  assert.ok(allProtocols.includes("https"));
});

test("the sanitize schema keeps links, emphasis, and code", () => {
  for (const tag of ["a", "em", "strong", "code", "pre"]) {
    assert.ok(tagNames.includes(tag), `expected ${tag} to survive`);
  }
  const hrefProtocols = (markdownSchema.protocols as { href?: string[] } | undefined)?.href ?? [];
  assert.ok(hrefProtocols.includes("https"));
});

test("code reference parsing accepts file-like path:line spans and rejects the rest", async () => {
  const { parseCodeRef } = await import("./comments.ts");
  assert.deepEqual(parseCodeRef("middleware.ts:14"), { path: "middleware.ts", start: 14, end: null, sha: null });
  assert.deepEqual(parseCodeRef("src/auth/tokens.ts:31-58"), { path: "src/auth/tokens.ts", start: 31, end: 58, sha: null });
  assert.deepEqual(parseCodeRef("auth.go:18@f0c3d21"), { path: "auth.go", start: 18, end: null, sha: "f0c3d21" });
  assert.equal(parseCodeRef("deny(\"no session\")"), null);
  assert.equal(parseCodeRef("localhost:5173"), null);
  assert.equal(parseCodeRef(""), null);
});
