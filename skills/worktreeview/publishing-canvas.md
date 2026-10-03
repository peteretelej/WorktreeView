# Publishing canvas documents

The Canvas tab is where agents publish rich, durable documents for the
human: walkthroughs ("how does this auth flow work?"), design notes,
checklists, diagrams. Documents are ordinary comments carrying a `kind`
slug, rendered in the app's Canvas tab with markdown, Mermaid diagrams,
and clickable code references. Anyone who can read comments can read
them, and anyone can reply, so a published explanation stays a
conversation.

## When to publish

- When the human asks: "explain how this auth works", "walk me through
  the request pipeline", "/worktreeview explain ..." - publish a
  walkthrough document.
- When an explanation you just wrote in chat is worth keeping: in the
  app it persists, renders, and others can build on it instead of
  scrolling a transcript.
- After landing a non-trivial change: a short walkthrough of the change
  helps every later reviewer. When you were not asked, prefer offering
  ("want this as a canvas document?") over posting uninvited; a canvas
  full of unrequested docs helps nobody.

## How to publish

One `create_comment` call, review-level (no file anchor), with `kind`:

- `repo_path`: from `list_repos`.
- `target_kind`: `"head"`, `target_key`: the exact head you are
  explaining. For a codebase tour that is the worktree's current head
  from `list_review_targets`; for a change walkthrough, the change's
  head.
- `base_sha`: the empty tree for codebase tours
  (`4b825dc642cb6eb9a060e54bf8d69288fbee4904` on SHA-1 repositories,
  which is nearly all of them; SHA-256 repositories compute their own
  via `git hash-object -t tree /dev/null`). For a change walkthrough,
  the change's base from the review in question.
- `kind`: a short slug - `walkthrough` for explanations, `notes` for
  working documents. Other slugs are allowed; the app badges whatever
  you send.
- `body`: the document (below).

Documents are pinned: line references resolve against the exact
revision you posted, forever. When the code moves far enough that the
tour misleads, publish a fresh one at the new head; the old thread stays
readable as history. Before publishing, check `list_canvas_docs` for an
existing document of the same scope and note supersession in the new
body ("supersedes the 3d-ago tour"). Edit your own documents only for
typo-level fixes; never re-base one in place.

## Writing the body

- Start with a `# Title` line: the library lists documents by it.
- Structure as ordered steps (`## 1 · Session read fails closed`) in
  reading order - the order a senior engineer narrates the flow, not
  file order.
- Reference code as backticked `` `path:line` `` or `` `path:start-end`
  `` spans; the app renders them as jump chips into the pinned revision.
  To narrate history, cite another revision explicitly:
  `` `auth.go:18@f0c3d21` `` - write the sha yourself and put the commit
  subject in prose ("previously overhauled in Fix login redirect").
- Diagrams where they genuinely help, never decoratively: fenced
  ```` ```mermaid ```` blocks (sequence, flowchart, state) render as
  pictures; agents reading the document see the source.
- Static checklists (`- [x]`, `- [ ]`) render read-only; tick them by
  editing your own document when the human reports progress.

## After publishing

The human reads the document in the Canvas tab, replies on the thread,
and may ask another agent to review your explanation. Work replies the
way you work any thread: read with `list_comments`, answer with
`reply_comment`, and take a critique seriously - a correction with a
severity badge is a review finding on your prose.
