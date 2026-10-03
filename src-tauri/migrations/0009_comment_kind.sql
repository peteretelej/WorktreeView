-- Canvas documents are kind-marked root comments; the vocabulary is
-- convention-level (the skill teaches "walkthrough", "notes"), so the
-- column stays a free short slug and unknown values stay forward-compatible.
ALTER TABLE comments ADD COLUMN kind TEXT;
