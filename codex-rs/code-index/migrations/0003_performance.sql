CREATE INDEX IF NOT EXISTS idx_chunks_path_lines
    ON chunks(path, start_line, end_line);

CREATE INDEX IF NOT EXISTS idx_chunks_symbol_lower
    ON chunks(lower(symbol));

CREATE INDEX IF NOT EXISTS idx_chunks_parent_symbol_lower
    ON chunks(lower(parent_symbol));

CREATE INDEX IF NOT EXISTS idx_files_content_hash
    ON files(content_hash);
