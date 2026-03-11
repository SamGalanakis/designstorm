CREATE TABLE IF NOT EXISTS design_sessions (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_design_sessions_owner_updated
  ON design_sessions (owner_user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS design_jobs (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  session_id UUID NOT NULL REFERENCES design_sessions(id) ON DELETE CASCADE,
  owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'completed', 'failed')),
  prompt TEXT NOT NULL,
  title TEXT NOT NULL,
  iterates_on_id UUID NULL REFERENCES storm_runs(id) ON DELETE SET NULL,
  result_run_id UUID NULL REFERENCES storm_runs(id) ON DELETE SET NULL,
  reference_snapshot_json JSONB NOT NULL DEFAULT '[]'::jsonb,
  error TEXT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  started_at TIMESTAMPTZ NULL,
  completed_at TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS idx_design_jobs_session_created
  ON design_jobs (session_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_design_jobs_owner_status
  ON design_jobs (owner_user_id, status, created_at DESC);

CREATE TABLE IF NOT EXISTS session_messages (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  session_id UUID NOT NULL REFERENCES design_sessions(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
  body TEXT NOT NULL,
  design_job_id UUID NULL REFERENCES design_jobs(id) ON DELETE SET NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_session_messages_session_created
  ON session_messages (session_id, created_at ASC);

CREATE TABLE IF NOT EXISTS reference_items (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  session_id UUID NOT NULL REFERENCES design_sessions(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('text', 'link', 'image')),
  title TEXT NOT NULL,
  content_json JSONB NOT NULL DEFAULT '{}'::jsonb,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_reference_items_session_created
  ON reference_items (session_id, created_at ASC);

ALTER TABLE storm_runs
  ADD COLUMN IF NOT EXISTS session_id UUID NULL REFERENCES design_sessions(id) ON DELETE SET NULL;

ALTER TABLE storm_runs
  ADD COLUMN IF NOT EXISTS iterates_on_id UUID NULL REFERENCES storm_runs(id) ON DELETE SET NULL;

UPDATE storm_runs
SET iterates_on_id = parent_ids[1]
WHERE iterates_on_id IS NULL
  AND array_length(parent_ids, 1) >= 1;

INSERT INTO design_sessions (id, owner_user_id, title, created_at, updated_at)
SELECT gen_random_uuid(), owner_user_id, 'Imported Session', MIN(created_at), MAX(created_at)
FROM storm_runs
WHERE session_id IS NULL
GROUP BY owner_user_id;

UPDATE storm_runs AS run
SET session_id = imported.id
FROM (
  SELECT DISTINCT ON (owner_user_id) owner_user_id, id
  FROM design_sessions
  WHERE title = 'Imported Session'
  ORDER BY owner_user_id, created_at ASC
) AS imported
WHERE run.session_id IS NULL
  AND run.owner_user_id = imported.owner_user_id;

DROP TABLE IF EXISTS board_edges;
DROP TABLE IF EXISTS board_nodes;
