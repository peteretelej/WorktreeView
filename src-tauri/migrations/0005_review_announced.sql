-- SQLite cannot alter an inline CHECK in place: the events table rebuilds
-- with the kind vocabulary extended by 'review_announced'. Existing rows
-- carry over byte-identical, and the 0004 index is recreated as shipped.
CREATE TABLE events_new (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_path TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN (
    'request_created','request_claimed','request_verdict',
    'request_re_requested','request_withdrawn','review_announced',
    'submission_delivered',
    'comment_posted','comment_replied','comment_resolved','comment_reopened',
    'surface_head_moved','repo_added')),
  base_sha TEXT,
  target_key TEXT,
  target_kind TEXT,
  request_id INTEGER REFERENCES review_requests(id) ON DELETE SET NULL,
  comment_id INTEGER REFERENCES comments(id) ON DELETE SET NULL,
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human','agent')),
  actor_name TEXT NOT NULL,
  summary TEXT NOT NULL CHECK (length(summary) <= 500),
  created_at INTEGER NOT NULL
);
INSERT INTO events_new SELECT * FROM events;
DROP TABLE events;
ALTER TABLE events_new RENAME TO events;
CREATE INDEX events_repo_id ON events(repo_path, id);
