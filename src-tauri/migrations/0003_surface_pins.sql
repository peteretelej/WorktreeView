ALTER TABLE retrospected_surfaces ADD COLUMN pinned_at INTEGER;
ALTER TABLE retrospected_surfaces ADD COLUMN origin TEXT NOT NULL DEFAULT 'review';

-- Repo-scoped rows whose repos row vanished (possible outside the app's own
-- flows) would abort the foreign-key rebuild below, so sweep them first.
DELETE FROM log_pages WHERE repo_path NOT IN (SELECT path FROM repos);
DELETE FROM retrospected_surfaces WHERE repo_path NOT IN (SELECT path FROM repos);

-- Baseline sweep of content rows no cached page references; commits and
-- ancestry marks are shared across repos and are never cascaded.
DELETE FROM commits
WHERE sha NOT IN (
  SELECT value FROM log_pages, json_each(log_pages.commit_shas)
);
DELETE FROM ancestry_marks
WHERE commit_sha NOT IN (
  SELECT value FROM log_pages, json_each(log_pages.commit_shas)
);

CREATE TABLE log_pages_new (
  repo_path TEXT NOT NULL REFERENCES repos(path) ON DELETE CASCADE,
  start_sha TEXT NOT NULL,
  against_sha TEXT NOT NULL DEFAULT '',
  skip INTEGER NOT NULL,
  limit_value INTEGER NOT NULL,
  commit_shas TEXT NOT NULL,
  has_more INTEGER NOT NULL,
  PRIMARY KEY (repo_path, start_sha, against_sha, skip, limit_value)
);
INSERT INTO log_pages_new (repo_path, start_sha, against_sha, skip, limit_value, commit_shas, has_more)
SELECT repo_path, start_sha, against_sha, skip, limit_value, commit_shas, has_more FROM log_pages;
DROP TABLE log_pages;
ALTER TABLE log_pages_new RENAME TO log_pages;

CREATE TABLE retrospected_surfaces_new (
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
INSERT INTO retrospected_surfaces_new (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at, pinned_at, origin)
SELECT repo_path, kind, identity_key, label, detail, head_sha, last_seen_at, pinned_at, origin FROM retrospected_surfaces;
DROP TABLE retrospected_surfaces;
ALTER TABLE retrospected_surfaces_new RENAME TO retrospected_surfaces;
