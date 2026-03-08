-- Widen node_type CHECK to support new node types
ALTER TABLE board_nodes DROP CONSTRAINT IF EXISTS board_nodes_node_type_check;
ALTER TABLE board_nodes ADD CONSTRAINT board_nodes_node_type_check
  CHECK (node_type IN (
    'entropy', 'user_input', 'generate',
    'color_palette', 'pick_k', 'set', 'image',
    'int_value', 'float_value', 'string_value', 'bool_value',
    'font'
  ));

-- Drop old edge type CHECK constraints (app validation is sufficient)
ALTER TABLE board_edges DROP CONSTRAINT IF EXISTS board_edges_source_type_check;
ALTER TABLE board_edges DROP CONSTRAINT IF EXISTS board_edges_target_type_check;

-- Add slot names to edges
ALTER TABLE board_edges
  ADD COLUMN IF NOT EXISTS source_slot TEXT NOT NULL DEFAULT 'out',
  ADD COLUMN IF NOT EXISTS target_slot TEXT NOT NULL DEFAULT 'sources';

-- Replace old unique constraint with slot-aware one
ALTER TABLE board_edges DROP CONSTRAINT IF EXISTS board_edges_source_id_target_id_key;
CREATE UNIQUE INDEX IF NOT EXISTS idx_board_edges_unique_slot
  ON board_edges (source_id, source_slot, target_id, target_slot);
