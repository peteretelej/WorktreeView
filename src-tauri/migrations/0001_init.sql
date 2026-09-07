CREATE TABLE repos (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  last_opened_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  pinned_at INTEGER
);
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE commits (
  sha TEXT PRIMARY KEY,
  subject TEXT NOT NULL,
  author TEXT NOT NULL,
  date TEXT NOT NULL,
  refs TEXT NOT NULL,
  parents TEXT NOT NULL
);
CREATE TABLE log_pages (
  repo_path TEXT NOT NULL REFERENCES repos(path) ON DELETE CASCADE,
  start_sha TEXT NOT NULL,
  against_sha TEXT NOT NULL DEFAULT '',
  skip INTEGER NOT NULL,
  limit_value INTEGER NOT NULL,
  commit_shas TEXT NOT NULL,
  has_more INTEGER NOT NULL,
  PRIMARY KEY (repo_path, start_sha, against_sha, skip, limit_value)
);
CREATE TABLE ancestry_marks (
  commit_sha TEXT NOT NULL,
  against_sha TEXT NOT NULL,
  non_ancestor INTEGER NOT NULL,
  PRIMARY KEY (commit_sha, against_sha)
);
CREATE TABLE retrospected_surfaces (
  repo_path TEXT NOT NULL REFERENCES repos(path) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('worktree','branch')),
  identity_key TEXT NOT NULL,
  label TEXT NOT NULL,
  detail TEXT NOT NULL,
  head_sha TEXT NOT NULL,
  last_seen_at INTEGER NOT NULL,
  pinned_at INTEGER,
  origin TEXT NOT NULL DEFAULT 'review',
  PRIMARY KEY (repo_path, kind, identity_key)
);
CREATE TABLE reviews (
  id INTEGER PRIMARY KEY,
  repo_path TEXT NOT NULL REFERENCES repos(path) ON DELETE CASCADE,
  base_sha TEXT NOT NULL,
  target_key TEXT NOT NULL,
  target_kind TEXT NOT NULL CHECK (target_kind IN ('worktree','head')),
  created_at INTEGER NOT NULL,
  UNIQUE (repo_path, base_sha, target_key, target_kind)
);
CREATE TABLE submissions (
  id INTEGER PRIMARY KEY,
  review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  agent_name TEXT NOT NULL,
  agent_model TEXT NOT NULL,
  command_context TEXT,
  sections_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE comments (
  id INTEGER PRIMARY KEY,
  review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  parent_id INTEGER REFERENCES comments(id) ON DELETE CASCADE,
  author_kind TEXT NOT NULL CHECK (author_kind IN ('human','agent')),
  author_name TEXT NOT NULL,
  author_model TEXT,
  body TEXT NOT NULL,
  file_path TEXT,
  side TEXT CHECK (side IN ('LEFT','RIGHT')),
  start_line INTEGER,
  end_line INTEGER,
  anchor_hash TEXT,
  snippet TEXT,
  severity TEXT CHECK (severity IS NULL OR severity IN ('P0','P1','P2','P3')),
  submission_id INTEGER REFERENCES submissions(id) ON DELETE SET NULL,
  resolved_at INTEGER,
  edited_at INTEGER,
  created_at INTEGER NOT NULL,
  CHECK (
    (file_path IS NULL AND side IS NULL AND start_line IS NULL AND end_line IS NULL)
    OR (file_path IS NOT NULL AND side IS NULL AND start_line IS NULL AND end_line IS NULL)
    OR (file_path IS NOT NULL AND side IS NOT NULL AND start_line IS NOT NULL)
  ),
  CHECK (end_line IS NULL OR end_line >= start_line)
);
