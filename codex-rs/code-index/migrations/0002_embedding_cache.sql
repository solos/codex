CREATE TABLE IF NOT EXISTS embedding_cache (
    content_hash TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    embedding BLOB NOT NULL,
    created_at_unix_ms INTEGER NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (content_hash, provider, model)
);

CREATE INDEX IF NOT EXISTS idx_embedding_cache_provider_model
    ON embedding_cache(provider, model, updated_at_unix_ms);
