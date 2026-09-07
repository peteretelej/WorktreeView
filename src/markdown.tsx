import Markdown from "react-markdown";
import rehypeSanitize from "rehype-sanitize";
import { markdownSchema } from "./markdown-schema.ts";

// Shared sanitized markdown renderer for comment bodies; the schema is the
// audited default, so raw HTML and event handlers never reach the DOM.
export function CommentBody({ text }: { text: string }) {
  return <div className="comment-body"><Markdown rehypePlugins={[[rehypeSanitize, markdownSchema]]}>{text}</Markdown></div>;
}
