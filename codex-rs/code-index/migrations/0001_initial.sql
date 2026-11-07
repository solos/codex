CREATE TABLE IF NOT EXISTS index_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL,
    indexed_at_unix_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    language TEXT NOT NULL,
    symbol TEXT,
    parent_symbol TEXT,
    kind TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    UNIQUE(file_id, start_line, end_line, content_hash)
);

CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);
CREATE INDEX IF NOT EXISTS idx_chunks_file_id ON chunks(file_id);
CREATE INDEX IF NOT EXISTS idx_chunks_symbol ON chunks(symbol);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    path,
    symbol,
    parent_symbol,
    content,
    content = 'chunks',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_trigram USING fts5(
    path,
    symbol,
    parent_symbol,
    content,
    content = 'chunks',
    content_rowid = 'id',
    tokenize = 'trigram'
);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, path, symbol, parent_symbol, content)
    VALUES (new.id, new.path, new.symbol, new.parent_symbol, new.content);
    INSERT INTO chunks_trigram(rowid, path, symbol, parent_symbol, content)
    VALUES (new.id, new.path, new.symbol, new.parent_symbol, new.content);
END;

CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, path, symbol, parent_symbol, content)
    VALUES ('delete', old.id, old.path, old.symbol, old.parent_symbol, old.content);
    INSERT INTO chunks_trigram(chunks_trigram, rowid, path, symbol, parent_symbol, content)
    VALUES ('delete', old.id, old.path, old.symbol, old.parent_symbol, old.content);
END;

CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, path, symbol, parent_symbol, content)
    VALUES ('delete', old.id, old.path, old.symbol, old.parent_symbol, old.content);
    INSERT INTO chunks_trigram(chunks_trigram, rowid, path, symbol, parent_symbol, content)
    VALUES ('delete', old.id, old.path, old.symbol, old.parent_symbol, old.content);
    INSERT INTO chunks_fts(rowid, path, symbol, parent_symbol, content)
    VALUES (new.id, new.path, new.symbol, new.parent_symbol, new.content);
    INSERT INTO chunks_trigram(rowid, path, symbol, parent_symbol, content)
    VALUES (new.id, new.path, new.symbol, new.parent_symbol, new.content);
END;
