CREATE TABLE review_requests (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_path TEXT NOT NULL REFERENCES repos(path) ON DELETE CASCADE,
  base_sha TEXT NOT NULL,
  target_key TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  requester_token_id INTEGER REFERENCES agent_tokens(id) ON DELETE SET NULL,
  status TEXT NOT NULL,
  note TEXT NOT NULL,
  lenses TEXT NOT NULL,
  reviewers TEXT NOT NULL,
  max_rounds INTEGER NOT NULL DEFAULT 2,
  round INTEGER NOT NULL DEFAULT 0,
  head_sha TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX review_requests_identity
  ON review_requests(repo_path, base_sha, target_key, target_kind);
