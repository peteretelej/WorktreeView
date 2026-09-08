// Pure textarea transforms behind the markdown composer's toolbar and
// keyboard shortcuts. Each returns the next value plus the selection the
// textarea should restore, so the component stays stateless about text.

// Blocks that toggle per line (quotes, lists) operate on whole lines even
// when the selection only covers part of one.
export type Edit = { value: string; start: number; end: number };
export type TextState = { value: string; selectionStart: number; selectionEnd: number };

export function wrapSelection(state: TextState, prefix: string, suffix: string): Edit {
  const { value, selectionStart, selectionEnd } = state;
  const selected = value.slice(selectionStart, selectionEnd);
  if (selected.startsWith(prefix) && selected.endsWith(suffix) && selected.length >= prefix.length + suffix.length) {
    const inner = selected.slice(prefix.length, selected.length - suffix.length);
    const value2 = value.slice(0, selectionStart) + inner + value.slice(selectionEnd);
    return { value: value2, start: selectionStart, end: selectionStart + inner.length };
  }
  const value2 = value.slice(0, selectionStart) + prefix + selected + suffix + value.slice(selectionEnd);
  if (selected === "") return { value: value2, start: selectionStart + prefix.length, end: selectionStart + prefix.length };
  return { value: value2, start: selectionStart + prefix.length, end: selectionStart + prefix.length + selected.length };
}

function lineBlockOf(value: string, selectionStart: number, selectionEnd: number) {
  const start = value.lastIndexOf("\n", selectionStart - 1) + 1;
  const newline = value.indexOf("\n", selectionEnd);
  const end = newline === -1 ? value.length : newline;
  return { start, end, lines: value.slice(start, end).split("\n") };
}

// Ordered-list numbering is a function of the line index; bullets and
// quotes pass a constant prefix.
export function toggleLinePrefix(state: TextState, prefix: string | ((index: number) => string)): Edit {
  const { value, selectionStart, selectionEnd } = state;
  const block = lineBlockOf(value, selectionStart, selectionEnd);
  const at = (index: number) => (typeof prefix === "function" ? prefix(index) : prefix);
  const stripped = block.lines.map((line, index) => line.startsWith(at(index)) ? line.slice(at(index).length) : line);
  const allPrefixed = block.lines.every((line, index) => line.startsWith(at(index)));
  const lines = allPrefixed ? stripped : block.lines.map((line, index) => at(index) + line);
  const value2 = value.slice(0, block.start) + lines.join("\n") + value.slice(block.end);
  return { value: value2, start: block.start, end: block.start + lines.join("\n").length };
}

export function insertLink(state: TextState): Edit {
  const { value, selectionStart, selectionEnd } = state;
  const selected = value.slice(selectionStart, selectionEnd);
  // A URL selection becomes the target and the label becomes editable;
  // anything else becomes the label with the target selected for typing.
  if (/^\w+:\/\//.test(selected) || /^www\./.test(selected)) {
    const text = `[link](${selected})`;
    const value2 = value.slice(0, selectionStart) + text + value.slice(selectionEnd);
    return { value: value2, start: selectionStart + 1, end: selectionStart + 5 };
  }
  const text = `[${selected || "text"}](url)`;
  const value2 = value.slice(0, selectionStart) + text + value.slice(selectionEnd);
  const urlStart = selectionStart + selected.length + 3;
  return { value: value2, start: urlStart, end: urlStart + 3 };
}
