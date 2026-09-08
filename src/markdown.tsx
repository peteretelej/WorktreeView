import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import { markdownSchema } from "./markdown-schema.ts";

// Shared sanitized markdown renderer for comment bodies; GFM extensions
// (tables, strikethrough, task lists) match what agents routinely emit, and
// the schema is the audited default, so raw HTML and event handlers never
// reach the DOM.
export function CommentBody({ text }: { text: string }) {
  return <div className="comment-body"><Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[[rehypeSanitize, markdownSchema]]}>{text}</Markdown></div>;
}
