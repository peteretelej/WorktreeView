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
  repo_path TEXT NOT NULL,
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
