ALTER TABLE board_edges
  ADD COLUMN source_anchor_side TEXT,
  ADD COLUMN source_anchor_t DOUBLE PRECISION,
  ADD COLUMN target_anchor_side TEXT,
  ADD COLUMN target_anchor_t DOUBLE PRECISION;

ALTER TABLE board_nodes
  ADD COLUMN width DOUBLE PRECISION,
  ADD COLUMN height DOUBLE PRECISION;
