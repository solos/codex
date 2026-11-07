CREATE INDEX IF NOT EXISTS idx_chunks_content_hash
    ON chunks(content_hash);
