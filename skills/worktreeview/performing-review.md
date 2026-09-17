# Performing review in WorktreeView

You are a **reviewer**: another agent (or the human) asked the fleet for a
review, and you are picking one up. The app is the venue; how you review
is your own craft. This page covers the pickup protocol and the etiquette
that makes several reviewers work.

## Pick up a request

Discovery is poll-based - the API has no subscriptions - so check the
queue on a sparse cadence: once at task start, then every few minutes
while idle. The listener serves connections serially, so a tight poll
loop only queues behind itself.

Call `list_review_requests` with **no filters**: it answers the cross-repo
open queue, each row carrying the identity, `status`, `note`, `lenses`,
`round`/`max_rounds`, the recorded `head_sha`, `age_ms`, `comment_count`,
and unresolved-finding counts. That one call tells you what is waiting,
for whom, and how badly. `requester` names the asking token, or `human`
for a human-initiated request.

Confirm scope with the user before diving in ("A review on X was
requested with security and tests lenses; taking it?"). When the user
asked you to check WorktreeView, sensible defaults from the request's
lenses and note are confirmation enough.

## Claim, or just start

How you enter a review depends on where the ask came from:

- **Verbal ask, no request row**: when the human (or another agent) asked
  you directly and nothing sits in the queue, announce the start with
  `announce_review` (the review identity plus the `head_sha` you are
  reviewing, optional `note`). It records your token as the requester and
  puts the request straight into review, so a later delivery settles it
  like a claim would.
- **Your own request**: starting work on a row you created through
  `request_review` uses the same `announce_review`; it moves your request
  into review in place instead of stacking a second row.
- **Queue pickup**: picking up someone else's request from
  `list_review_requests` claims it (`update_review_request`, `action:
  "claim"`). The claim already narrates the start, so the pickup flow
  needs no second row - do not announce on top of another party's pickup
  row; announcing keys on your own token and would stack a second open
  row beside theirs.

Claiming marks a request as taken (`update_review_request`, `action:
"claim"`):

- Named reviewers: when the request names reviewer tokens, only those
  tokens can claim it. If you are not named, leave it alone.
- Open pickup: when no reviewers are named, any agent can claim.

A requested review has one claim in it; after that the request is in
review and further claims are refused. One reviewer drives each request.
You can still submit findings without claiming - the request advances on
submissions - but do not shoulder in on a claim someone else holds.

## Fetch, review in your own space

The app never performs Git for you and you never review inside someone
else's checkout. Work from your own clone or worktree: fetch the
repository, check out the request's recorded `head_sha` (a detached HEAD
is fine for review), and diff it against the request's `base_sha`. Repos
are local and co-located; if you lack the objects, fetch from the shared
remote - or ask the human to arrange access - never through the app.

If the worktree behind the request has moved past the recorded head, say
so in a comment rather than reviewing a moving target.

## Submit blind, then collaborate

Deliver findings as a submission through the raw JSON-RPC face
(`post_review`, same endpoint `POST /`, same bearer token): sections for
the summary, one finding per issue with `P0` (critical) through `P3`
(nit) priorities, anchored to files and lines where possible. P0 and P1
are blocking: a blocking finding sets the request to changes-requested.

**Blind first pass**: before you submit, do not read the other reviewers'
findings on this request - `list_comments` and `list_submissions` answers
include them, so form your own view first and submit before you go
reading. Independent reviews are the point of a fleet; anchoring on
someone else's findings only doubles one opinion.

After submitting, collaboration opens up:

- Engage: answer the requester's replies, concede when their evidence
  beats yours, push back when it does not.
- Disagreement stays visible: leave contested threads unresolved rather
  than resolving them to end an argument.
- Verdicts are sticky within a round. A clean submission after a blocking
  one does not approve the request; only the requester's re-request
  resets the round. If the fixes look good, say so in a thread - the
  requester re-requests and the next round is yours to approve.
- Explicit verdicts (`approve`, `request_changes` via
  `update_review_request`) close a round and are open to any agent token
  on a request that is in review. Use `approve` for the clean pass that
  ends a converged review.

The same etiquette applies with human reviewers: they see your findings
live and reply in the same threads. Write for them, not just for the
other agents.
