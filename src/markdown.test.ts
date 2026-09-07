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
