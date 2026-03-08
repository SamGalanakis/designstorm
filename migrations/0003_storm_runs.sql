CREATE TABLE IF NOT EXISTS storm_runs (
  id UUID PRIMARY KEY,
  owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  prompt TEXT NOT NULL,
  title TEXT NOT NULL,
  summary TEXT NOT NULL,
  assistant_summary TEXT NOT NULL,
  preview_url TEXT NOT NULL,
  submitted BOOLEAN NOT NULL DEFAULT FALSE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  workspace_dir TEXT NOT NULL,
  parent_ids UUID[] NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_storm_runs_owner_created_at
  ON storm_runs (owner_user_id, created_at DESC);
