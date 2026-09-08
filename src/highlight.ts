// Main-thread API for diff token highlighting. The Shiki engine lives in
// highlight.worker.ts so grammar CPU can never block the UI thread; this
// module owns language detection, the worker RPC, and the token cache.
// Shiki is never imported here.

export type TokenWithVariants = { content: string; variants?: Record<string, { color?: string; fontStyle?: number }> };
export type HighlightToken = { content: string; light?: string; dark?: string; italic: boolean; bold: boolean; underline: boolean };
export type TokenLine = HighlightToken[];
export type HighlightHunk = { old: TokenLine[]; new: TokenLine[] };

const FONT_ITALIC = 1;
const FONT_BOLD = 2;
const FONT_UNDERLINE = 4;
export const HUNK_TOKENIZE_LINE_LIMIT = 10_000;
const TOKEN_CACHE_LIMIT = 512;

export function toTokenLine(tokens: TokenWithVariants[]): TokenLine {
  return tokens.map((token) => {
    const light = token.variants?.light;
    const dark = token.variants?.dark;
    const fontStyle = (light?.fontStyle ?? 0) | (dark?.fontStyle ?? 0);
    return {
      content: token.content,
      light: light?.color,
      dark: dark?.color,
      italic: (fontStyle & FONT_ITALIC) !== 0,
      bold: (fontStyle & FONT_BOLD) !== 0,
      underline: (fontStyle & FONT_UNDERLINE) !== 0,
    };
  });
}

// Wide-breadth language coverage; every id must exist in shiki's
// bundledLanguages (verified programmatically). Unknown extensions and
// filenames fall through to plain text.
const LANGUAGE_BY_EXTENSION: Record<string, string> = {
  js: "javascript", mjs: "javascript", cjs: "javascript", jsx: "jsx",
  ts: "typescript", mts: "typescript", cts: "typescript", tsx: "tsx",
  vue: "vue", svelte: "svelte", astro: "astro",
  html: "html", htm: "html", xhtml: "html",
  xml: "xml", svg: "xml", xsl: "xml", xslt: "xml", plist: "xml",
  css: "css", scss: "scss", sass: "sass", less: "less", postcss: "postcss",
  json: "json", jsonc: "jsonc", json5: "json5",
  yaml: "yaml", yml: "yaml", toml: "toml",
  ini: "ini", cfg: "ini", conf: "ini", properties: "ini", env: "ini",
  md: "markdown", markdown: "markdown", mdx: "mdx",
  tex: "latex", latex: "latex", rst: "rst",
  sh: "shellscript", bash: "shellscript", zsh: "shellscript", fish: "fish",
  ps1: "powershell", psm1: "powershell", psd1: "powershell",
  bat: "bat", cmd: "bat",
  sql: "sql", graphql: "graphql", gql: "graphql", proto: "proto",
  diff: "diff", patch: "diff",
  go: "go", rs: "rust", py: "python", pyi: "python",
  java: "java", kt: "kotlin", kts: "kotlin", scala: "scala",
  groovy: "groovy", gradle: "groovy",
  cs: "csharp", fs: "fsharp", fsi: "fsharp", fsx: "fsharp", vb: "vb",
  c: "c", h: "c",
  cc: "cpp", cpp: "cpp", cxx: "cpp", hpp: "cpp", hh: "cpp", hxx: "cpp", ino: "cpp",
  m: "objc", mm: "objc",
  rb: "ruby", erb: "erb", php: "php", pl: "perl", pm: "perl", lua: "lua",
  r: "r", jl: "julia", dart: "dart", swift: "swift", hs: "haskell",
  ex: "elixir", exs: "elixir", erl: "erlang", hrl: "erlang",
  clj: "clojure", cljs: "clojure", cljc: "clojure", edn: "clojure",
  coffee: "coffeescript", ml: "ocaml", mli: "ocaml", pas: "pascal",
  tcl: "tcl", awk: "awk", zig: "zig", nim: "nim", sol: "solidity",
  wgsl: "wgsl", hlsl: "hlsl", vim: "viml", vimrc: "viml",
  tf: "terraform", tfvars: "terraform", hcl: "hcl",
  prisma: "prisma",
};

// Extensionless files that map to a language.
const LANGUAGE_BY_FILENAME: Record<string, string> = {
  dockerfile: "dockerfile",
  makefile: "makefile",
  gnumakefile: "makefile",
  brewfile: "ruby",
  gemfile: "ruby",
  rakefile: "ruby",
};

export function languageForPath(path: string): string | null {
  const name = path.split(/[\\/]/).pop() ?? "";
  const lower = name.toLowerCase();
  if (LANGUAGE_BY_FILENAME[lower]) return LANGUAGE_BY_FILENAME[lower];
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return null;
  return LANGUAGE_BY_EXTENSION[name.slice(dot + 1).toLowerCase()] ?? null;
}

// Splits a hunk's raw lines into per-side source lines in order, dropping
// "\ No newline" metadata lines; both sides keep their own numbering so
// tokens zip back onto the DiffLine objects they came from.
export function hunkSideSources<T extends { text: string }>(lines: T[]): { old: T[]; new: T[] } {
  const old: T[] = [];
  const next: T[] = [];
  for (const line of lines) {
    const prefix = line.text[0];
    if (prefix === "\\") continue;
    if (prefix === "-") old.push(line);
    else if (prefix === "+") next.push(line);
    else { old.push(line); next.push(line); }
  }
  return { old, new: next };
}

export function sideContent(line: { text: string }): string {
  return line.text.slice(1);
}

// Must match the `tab-size` rule on `.diff-line` in App.css so whitespace
// glyphs land on the same stops the hidden renderer's raw tabs use.
export const DIFF_TAB_WIDTH = 4;

// Splits token content into visible-whitespace parts matching the plain-text
// renderer's line semantics: tabs become glyphs anywhere, but a trailing
// space run only becomes glyphs when the token ends the line (the plain
// renderer's ` +$` anchors at the line, not the token). Each tab renders as
// an arrow padded to the stop the hidden mode would advance to, so toggling
// whitespace never shifts line geometry; `column` is the 0-based column the
// content starts at (the diff marker occupies one).
export function splitWhitespace(content: string, atLineEnd: boolean, column: number): { parts: Array<{ glyph?: string; text?: string }>; endColumn: number } {
  const pattern = atLineEnd ? /\t| +$/g : /\t/g;
  const parts: Array<{ glyph?: string; text?: string }> = [];
  let last = 0;
  let endColumn = column;
  for (let match = pattern.exec(content); match !== null; match = pattern.exec(content)) {
    if (match.index > last) {
      const text = content.slice(last, match.index);
      parts.push({ text });
      endColumn += text.length;
    }
    if (match[0].startsWith("\t")) {
      const width = DIFF_TAB_WIDTH - (endColumn % DIFF_TAB_WIDTH);
      parts.push({ glyph: "→" + " ".repeat(width - 1) });
      endColumn += width;
    } else {
      parts.push({ glyph: "·".repeat(match[0].length) });
      endColumn += match[0].length;
    }
    last = pattern.lastIndex;
  }
  if (last === 0) return { parts: content ? [{ text: content }] : [], endColumn: column + content.length };
  if (last < content.length) {
    const text = content.slice(last);
    parts.push({ text });
    endColumn += text.length;
  }
  return { parts, endColumn };
}

type PendingRequest = { resolve: (result: HighlightHunk | null) => void; isCancelled?: () => boolean };

// Wire contract with highlight.worker.ts, shared so both sides stay checked.
export type TokenizeRequest = { id: number; kind: "tokenize"; lines: string[]; lang: string };
export type TokenizeResponse = { id: number; result: HighlightHunk | null };

let worker: Worker | null = null;
let workerFailed = false;
let nextRequestId = 1;
const pendingRequests = new Map<number, PendingRequest>();
const tokenCache = new Map<string, Promise<HighlightHunk | null>>();

function getWorker(): Worker | null {
  if (workerFailed) return null;
  if (!worker) {
    try {
      worker = new Worker(new URL("./highlight.worker.ts", import.meta.url), { type: "module" });
      worker.onmessage = (event) => {
        const { id, result } = (event.data as TokenizeResponse);
        const request = pendingRequests.get(id);
        if (!request) return;
        pendingRequests.delete(id);
        request.resolve(request.isCancelled?.() ? null : result);
      };
      worker.onerror = () => {
        for (const request of pendingRequests.values()) request.resolve(null);
        pendingRequests.clear();
        worker = null;
        workerFailed = true;
      };
    } catch {
      workerFailed = true;
    }
  }
  return worker;
}

// Tokenizes both sides of one whole hunk in the worker so grammar state
// stays coherent across patch pages; resolves null when the language cannot
// tokenize (oversized hunk, engine or grammar failure) or when isCancelled
// reports the requester went away, so callers render plain text. Cancelled
// attempts are dropped from the cache so a later view retries. Cached per
// content key; results are keyed back onto the original line objects by the
// caller via hunkSideSources.
export function tokenizeHunk(lines: Array<{ text: string }>, lang: string, isCancelled?: () => boolean): Promise<HighlightHunk | null> {
  // The key is the real content, not a hash: a hash collision would render
  // one hunk's tokens (and text) over another's lines, and the bounded cache
  // makes full-content keys affordable.
  const key = `${lang}\0${lines.map((line) => line.text).join("\n")}`;
  const cached = tokenCache.get(key);
  if (cached) return cached;
  const promise = new Promise<HighlightHunk | null>((resolve) => {
    if (isCancelled?.()) return resolve(null);
    const worker = getWorker();
    if (!worker) return resolve(null);
    const id = nextRequestId++;
    pendingRequests.set(id, { resolve, isCancelled });
    worker.postMessage({ id, kind: "tokenize", lines: lines.map((line) => line.text), lang } satisfies TokenizeRequest);
  });
  tokenCache.set(key, promise);
  void promise.then((result) => {
    if (result === null && tokenCache.get(key) === promise) tokenCache.delete(key);
  });
  if (tokenCache.size > TOKEN_CACHE_LIMIT) {
    for (const oldest of tokenCache.keys()) {
      tokenCache.delete(oldest);
      if (tokenCache.size <= TOKEN_CACHE_LIMIT) break;
    }
  }
  return promise;
}
