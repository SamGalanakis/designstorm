-- Add draw node type
ALTER TABLE board_nodes DROP CONSTRAINT IF EXISTS board_nodes_node_type_check;
ALTER TABLE board_nodes ADD CONSTRAINT board_nodes_node_type_check
  CHECK (node_type IN (
    'entropy', 'user_input', 'generate',
    'color_palette', 'color', 'pick_k', 'set', 'image', 'draw',
    'int_value', 'float_value', 'string_value', 'bool_value',
    'font'
  ));
