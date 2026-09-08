import { test } from "node:test";
import assert from "node:assert/strict";
import { hunkSideSources, languageForPath, sideContent, splitWhitespace, tokenizeHunk } from "./highlight.ts";

function line(text: string) {
  return { text, oldLine: null, newLine: null };
}

test("file extensions map to the supported language set", () => {
  assert.equal(languageForPath("src/App.tsx"), "tsx");
  assert.equal(languageForPath("src/diff.ts"), "typescript");
  assert.equal(languageForPath("bin/cli.mjs"), "javascript");
  assert.equal(languageForPath("web/comp.jsx"), "jsx");
  assert.equal(languageForPath("main.go"), "go");
  assert.equal(languageForPath("Program.cs"), "csharp");
  assert.equal(languageForPath("alloc.c"), "c");
  assert.equal(languageForPath("alloc.h"), "c");
  assert.equal(languageForPath("graph.cc"), "cpp");
  assert.equal(languageForPath("graph.CPP"), "cpp");
  assert.equal(languageForPath("graph.hpp"), "cpp");
  assert.equal(languageForPath("tool.pyi"), "python");
  assert.equal(languageForPath("Main.kt"), "kotlin");
  assert.equal(languageForPath("Lib.kts"), "kotlin");
  assert.equal(languageForPath("App.java"), "java");
  assert.equal(languageForPath("src-tauri/src/store.rs"), "rust");
  assert.equal(languageForPath("styles/main.css"), "css");
  assert.equal(languageForPath("notes.md"), "markdown");
  assert.equal(languageForPath("config/settings.json"), "json");
  assert.equal(languageForPath("deploy/app.yml"), "yaml");
  assert.equal(languageForPath("Cargo.toml"), "toml");
  assert.equal(languageForPath("review.patch"), "diff");
  assert.equal(languageForPath("scripts/bootstrap.sh"), "shellscript");
});

test("extensionless tool files map by filename", () => {
  assert.equal(languageForPath("deploy/Dockerfile"), "dockerfile");
  assert.equal(languageForPath("Makefile"), "makefile");
  assert.equal(languageForPath("src/Makefile.am"), null);
});

test("files without a supported extension stay unhighlighted", () => {
  assert.equal(languageForPath(".gitignore"), null);
  assert.equal(languageForPath("README"), null);
  assert.equal(languageForPath(""), null);
});

test("windows-style path separators still expose the extension", () => {
  assert.equal(languageForPath("src\\app\\Main.kt"), "kotlin");
});

test("hunk sides split by marker and keep order", () => {
  const context = line(" unchanged");
  const removal = line("-removed");
  const addition = line("+added");
  const meta = line("\\ No newline at end of file");
  const sides = hunkSideSources([context, removal, addition, meta]);
  assert.deepEqual(sides.old, [context, removal]);
  assert.deepEqual(sides.new, [context, addition]);
});

test("hunk side content strips the diff marker", () => {
  assert.equal(sideContent(line("+const x = 1;")), "const x = 1;");
  assert.equal(sideContent(line(" keep")), "keep");
});

test("splitWhitespace keeps plain text untouched as one part", () => {
  assert.deepEqual(splitWhitespace("hello", true, 0), { parts: [{ text: "hello" }], endColumn: 5 });
  assert.deepEqual(splitWhitespace("", true, 0), { parts: [], endColumn: 0 });
});

test("splitWhitespace renders line-final spaces as glyphs", () => {
  assert.deepEqual(splitWhitespace("x  ", true, 0), { parts: [{ text: "x" }, { glyph: "··" }], endColumn: 3 });
  assert.deepEqual(splitWhitespace("a  b  ", true, 0), { parts: [{ text: "a  b" }, { glyph: "··" }], endColumn: 6 });
});

test("splitWhitespace only glyphs spaces before the line end", () => {
  assert.deepEqual(splitWhitespace("x  ", false, 0), { parts: [{ text: "x  " }], endColumn: 3 });
});

test("splitWhitespace pads tabs to the next 4-column stop", () => {
  assert.deepEqual(splitWhitespace("\t", true, 0), { parts: [{ glyph: "→   " }], endColumn: 4 });
  assert.deepEqual(splitWhitespace("\t", true, 1), { parts: [{ glyph: "→  " }], endColumn: 4 });
  assert.deepEqual(splitWhitespace("\t", true, 3), { parts: [{ glyph: "→" }], endColumn: 4 });
  assert.deepEqual(splitWhitespace("\t", true, 4), { parts: [{ glyph: "→   " }], endColumn: 8 });
});

test("splitWhitespace pads consecutive tabs from the running column", () => {
  assert.deepEqual(
    splitWhitespace("a\t\tb", true, 0),
    { parts: [{ text: "a" }, { glyph: "→  " }, { glyph: "→   " }, { text: "b" }], endColumn: 9 },
  );
  assert.deepEqual(splitWhitespace("\t\t", true, 1), { parts: [{ glyph: "→  " }, { glyph: "→   " }], endColumn: 8 });
});

test("splitWhitespace threads the column across a line with tabs", () => {
  const first = splitWhitespace("a\tb", true, 0);
  assert.deepEqual(first, { parts: [{ text: "a" }, { glyph: "→  " }, { text: "b" }], endColumn: 5 });
  const second = splitWhitespace("\tc", true, first.endColumn);
  assert.deepEqual(second, { parts: [{ glyph: "→  " }, { text: "c" }], endColumn: 9 });
});

test("hunks past the line cap tokenize to nothing instead of burning the engine", async () => {
  const oversized = [{ text: "@@ -1,2 +1,2 @@" }];
  for (let i = 0; i < 10_001; i++) oversized.push(line(`+line ${i}`));
  const result = await tokenizeHunk(oversized, "typescript");
  assert.equal(result, null);
});
