-- Drop runs.git_dir.
--
-- Added in 0004 to carry the bare repo path through the bootstrap
-- response to quire-ci; the quire.stdlib mirror helper was its only
-- consumer, and mirror has been removed from CI (server-side
-- mirroring already runs on every push). quire-ci runs against the
-- materialized workspace and derives sha/ref host-side via --git-dir
-- in local mode, so the column is dead.
--
-- No CHECK constraint or index references git_dir, so ALTER TABLE
-- DROP COLUMN suffices (SQLite 3.35.0+); no table recreation.

ALTER TABLE runs DROP COLUMN git_dir;
