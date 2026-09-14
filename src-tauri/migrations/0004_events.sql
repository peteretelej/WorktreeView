CREATE TABLE events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_path TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN (
    'request_created','request_claimed','request_verdict',
    'request_re_requested','request_withdrawn',
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
CREATE INDEX events_repo_id ON events(repo_path, id);
