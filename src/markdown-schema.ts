import { defaultSchema } from "rehype-sanitize";

// Comment bodies render through rehype-sanitize with its default schema.
// The constant lives in a plain .ts module so node --test (no JSX
// transform) can assert on the shipped security surface directly.
export const markdownSchema = defaultSchema;
