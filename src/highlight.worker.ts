// Worker entry for diff tokenization: Shiki runs here so grammar CPU can
// never block the UI thread. The main thread owns language detection, the
// token cache, and rendering; this file owns the engine. Requests are one
// hunk at a time; a superseded request still finishes (bounded by the hunk
// line cap and per-line budgets) and its result is discarded by the main
// thread. Guards: hunks beyond the line cap and lines over the length cap
// render as plain text; the per-line time limit stops pathological lines.

import { bundledLanguages } from "shiki/langs";
import { hunkSideSources, sideContent, toTokenLine, HUNK_TOKENIZE_LINE_LIMIT, type TokenLine, type TokenWithVariants, type TokenizeRequest } from "./highlight";
type ShikiHighlighter = {
  loadLanguage: (input: unknown) => Promise<void>;
  codeToTokensWithThemes: (code: string, options: Record<string, unknown>) => TokenWithVariants[][] | Promise<TokenWithVariants[][]>;
};

const TOKENIZE_MAX_LINE_LENGTH = 2_000;
const TOKENIZE_TIME_LIMIT_MS = 250;

type ThemePair = { light: unknown; dark: unknown };

let highlighterPromise: Promise<{ highlighter: ShikiHighlighter; themes: ThemePair }> | null = null;
const loadedGrammars = new Map<string, Promise<unknown>>();

function getHighlighter(): Promise<{ highlighter: ShikiHighlighter; themes: ThemePair }> {
  if (!highlighterPromise) {
    highlighterPromise = (async () => {
      const [{ createHighlighterCore }, { createOnigurumaEngine }, { default: light }, { default: dark }] = await Promise.all([
        import("shiki/core"),
        import("shiki/engine/oniguruma"),
        import("shiki/themes/github-light.mjs"),
        import("shiki/themes/github-dark.mjs"),
      ]);
      const themes: ThemePair = { light, dark };
      const highlighter = await createHighlighterCore({ themes: [light, dark], langs: [], engine: createOnigurumaEngine(import("shiki/wasm")) });
      return { highlighter: highlighter as unknown as ShikiHighlighter, themes };
    })();
    highlighterPromise.catch(() => { highlighterPromise = null; });
  }
  return highlighterPromise;
}

function ensureGrammar(highlighter: ShikiHighlighter, lang: string): Promise<unknown> {
  // The bundled registry covers every language shiki ships; each entry is a
  // lazy dynamic import, so a grammar loads on first encounter only.
  const grammar = (bundledLanguages as Record<string, unknown>)[lang];
  if (!grammar) return Promise.resolve(null);
  let pending = loadedGrammars.get(lang);
  if (!pending) {
    pending = highlighter.loadLanguage(grammar);
    pending.catch(() => loadedGrammars.delete(lang));
    loadedGrammars.set(lang, pending);
  }
  return pending;
}

async function tokenize(lines: Array<{ text: string }>, lang: string): Promise<{ old: TokenLine[]; new: TokenLine[] } | null> {
  const sides = hunkSideSources(lines);
  if (sides.old.length > HUNK_TOKENIZE_LINE_LIMIT || sides.new.length > HUNK_TOKENIZE_LINE_LIMIT) return null;
  const { highlighter, themes } = await getHighlighter();
  await ensureGrammar(highlighter, lang);
  const options = { lang, themes: { light: themes.light, dark: themes.dark }, defaultColor: false, tokenizeMaxLineLength: TOKENIZE_MAX_LINE_LENGTH, tokenizeTimeLimit: TOKENIZE_TIME_LIMIT_MS };
  const oldTokens = await highlighter.codeToTokensWithThemes(sides.old.map(sideContent).join("\n"), options);
  const newTokens = await highlighter.codeToTokensWithThemes(sides.new.map(sideContent).join("\n"), options);
  return { old: oldTokens.map(toTokenLine), new: newTokens.map(toTokenLine) };
}

const scope = self as unknown as Worker;
scope.onmessage = (event: MessageEvent) => {
  const request = event.data as TokenizeRequest;
  if (request.kind !== "tokenize") return;
  // A failure (engine, grammar, wasm) must still answer, or the main thread
  // waits forever on a promise that never resolves.
  void tokenize(request.lines.map((text) => ({ text })), request.lang)
    .catch((error) => {
      console.warn("[highlight] tokenization failed; rendering plain", error);
      return null;
    })
    .then((result) => scope.postMessage({ id: request.id, result }));
};
