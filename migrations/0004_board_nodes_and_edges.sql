CREATE TABLE IF NOT EXISTS board_nodes (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  node_type TEXT NOT NULL CHECK (node_type IN ('entropy', 'user_input', 'generate')),
  position_x DOUBLE PRECISION NOT NULL DEFAULT 0,
  position_y DOUBLE PRECISION NOT NULL DEFAULT 0,
  content JSONB NOT NULL DEFAULT '{}',
  locked BOOLEAN NOT NULL DEFAULT FALSE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_board_nodes_owner ON board_nodes (owner_user_id);

CREATE TABLE IF NOT EXISTS board_edges (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  source_id UUID NOT NULL,
  source_type TEXT NOT NULL CHECK (source_type IN ('design', 'entropy', 'user_input', 'generate')),
  target_id UUID NOT NULL,
  target_type TEXT NOT NULL CHECK (target_type IN ('design', 'entropy', 'user_input', 'generate')),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (source_id, target_id)
);
CREATE INDEX IF NOT EXISTS idx_board_edges_owner ON board_edges (owner_user_id);
CREATE INDEX IF NOT EXISTS idx_board_edges_target ON board_edges (target_id);
