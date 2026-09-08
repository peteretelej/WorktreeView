import { test } from "node:test";
import assert from "node:assert/strict";
import { insertLink, toggleLinePrefix, wrapSelection } from "./markdownEdit.ts";

function state(value: string, selectionStart: number, selectionEnd = selectionStart) {
  return { value, selectionStart, selectionEnd };
}

test("wrapSelection wraps the selected text and keeps it selected", () => {
  const edit = wrapSelection(state("say hello now", 4, 9), "**", "**");
  assert.equal(edit.value, "say **hello** now");
  assert.deepEqual([edit.start, edit.end], [6, 11]);
});

test("wrapSelection with no selection inserts the markers under the cursor", () => {
  const edit = wrapSelection(state("ab", 1), "`", "`");
  assert.equal(edit.value, "a``b");
  assert.deepEqual([edit.start, edit.end], [2, 2]);
});

test("wrapSelection toggles already-wrapped text back off", () => {
  const edit = wrapSelection(state("a **bold** b", 2, 10), "**", "**");
  assert.equal(edit.value, "a bold b");
  assert.deepEqual([edit.start, edit.end], [2, 6]);
});

test("toggleLinePrefix bullets whole lines from a partial selection", () => {
  const edit = toggleLinePrefix(state("one\ntwo\nthree", 1, 5), "- ");
  assert.equal(edit.value, "- one\n- two\nthree");
  assert.deepEqual([edit.start, edit.end], [0, 11]);
});

test("toggleLinePrefix removes the prefix when every line has it", () => {
  const edit = toggleLinePrefix(state("> a\n> b", 0, 6), "> ");
  assert.equal(edit.value, "a\nb");
});

test("toggleLinePrefix numbers lines with an index function", () => {
  const edit = toggleLinePrefix(state("a\nb\nc", 0, 5), (index) => `${index + 1}. `);
  assert.equal(edit.value, "1. a\n2. b\n3. c");
});

test("insertLink turns a text selection into the label and selects the url", () => {
  const edit = insertLink(state("see docs here", 4, 8));
  assert.equal(edit.value, "see [docs](url) here");
  assert.deepEqual([edit.start, edit.end], [11, 14]);
});

test("insertLink turns a url selection into the target and selects the label", () => {
  const edit = insertLink(state("see https://example.com now", 4, 23));
  assert.equal(edit.value, "see [link](https://example.com) now");
  assert.deepEqual([edit.start, edit.end], [5, 9]);
});

test("insertLink with no selection inserts a template", () => {
  const edit = insertLink(state("x", 1));
  assert.equal(edit.value, "x[text](url)");
  assert.deepEqual([edit.start, edit.end], [4, 7]);
});
