# Requesting review in WorktreeView

You finished a change and want the fleet to review it. You are the
**requester**: you ask, then you own the negotiation until the review
converges or a human takes over. The app records and relays; you do the
aggregating. Review method stays yours - this page covers the protocol
only.

## Request a review

Call `request_review` with the review identity and what you want
reviewed:

- `repo_path`, `base_sha`, `target_key`, `target_kind`: the review
  identity (from `list_repos` / `list_review_targets`; worktree reviews
  use the worktree path as `target_key` with `target_kind` `worktree`).
- `note` (optional, recommended): what changed, why, and what kind of
  review you need. Max 2000 characters; an empty request is a general
  "please review this" ask.
- `head_sha` (required): the exact head you want reviewed. Record it from
  your own worktree (`git rev-parse HEAD`), never from memory; the review
  keys on it and every re-request must differ from it.
- Optional `lenses` (`security`, `correctness`, `design`, `performance`,
  `tests`), optional named `reviewers` (agent token names; empty means
  open pickup), optional `max_rounds` (1-3, default 2).

Your token becomes the requester. Dedup is per identity: the same head on
your open request updates its note in place; a new head on your
never-claimed request refreshes it in place. A head that already received
changes is refused - re-request instead (below).

After requesting, stop changing the recorded head: commit further work on
new commits but expect reviewers to review the recorded `head_sha`, and
carry later work into the next round.

## Aggregate the findings

Watch the request on a sparse cadence with `list_review_requests` (filter
by `repo_path`): the row's `status`, `comment_count`, and
`unresolved_finding_counts` tell you whether anyone picked it up and how
much landed. When reviewers' submissions arrive (their findings appear as
comments on the identity), work them as the requester:

1. **Verify each finding** with your own skills and judgment before
   acting. A finding is input, not an order; confirm it against the code.
2. **Reply per thread** (`reply_comment` on the thread root): accept with
   evidence (what you changed, where) or rebut with evidence (why the
   code is right as is). Keep every reply substantive; "fixed" and "won't
   fix" alone do not carry the reasoning.
3. **Resolve what was addressed** (`resolve_thread`) so open threads mean
   outstanding work.
4. **Post a consolidated response** (a `post_review` submission with a
   brief section, or a review-level comment): what you accepted, what you
   rebutted and why, what remains.

Disagreement stays visible: never delete or edit reviewer findings, and
leave rebutted threads unresolved if the reviewer may want the last word.

## Re-request with a new head

After committing fixes, call `update_review_request` with `action:
"re_request"`, the request `id`, the new `head_sha`, and optionally a
replacement `note`:

- Re-request is **requester-only** (agents); the human can act on any
  request.
- The new head must differ from the head that received the changes.
- The round increments and the verdict resets: the request goes back to
  in review for another pass.

Within a round the first blocking verdict wins and is **sticky**: a
P0/P1 finding or an explicit request-changes sets `changes_requested`,
and later clean submissions do not flip it. Only your re-request resets
it. Do not argue a sticky verdict away with more submissions; fix or
rebut in threads, then re-request.

## Know the limits

- `withdraw` (also requester-only) abandons the request when the review
  is moot. Approved and withdrawn requests are settled.
- Rounds are bounded (your `max_rounds`, 1-3). A re-request past the
  budget is refused and the request surfaces as **needs human**: stop and
  escalate to the human instead of looping - summarize the open
  disagreement in the thread so they can arbitrate.
- Verdicts, claims, and refusals come from the app, not from reviewers.
  Argue in threads, never against the API.
