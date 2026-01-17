//! SQLite FTS5 schema definitions

pub const SCHEMA: &str = r#"
-- Pragmas for performance
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = 100000;
PRAGMA mmap_size = 30000000;
PRAGMA temp_store = MEMORY;

-- File tracking
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    mtime INTEGER NOT NULL DEFAULT 0,
    language TEXT,
    indexed_at INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);

-- Symbol storage (external content for FTS5)
CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    container TEXT,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);

-- FTS5 for symbol search (external content mode)
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    name,
    kind,
    container,
    content='symbols',
    content_rowid='id',
    tokenize="unicode61 tokenchars '_'"
);

-- Content lines storage
CREATE TABLE IF NOT EXISTS content_lines (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    line_num INTEGER NOT NULL,
    content TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_content_file ON content_lines(file_id);

-- FTS5 for content search
CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
    content,
    content='content_lines',
    content_rowid='id',
    tokenize="unicode61 tokenchars '_'"
);

-- Triggers for symbols_fts sync
CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, kind, container)
    VALUES (new.id, new.name, new.kind, new.container);
END;

CREATE TRIGGER IF NOT EXISTS symbols_ad AFTER DELETE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, container)
    VALUES ('delete', old.id, old.name, old.kind, old.container);
END;

-- Triggers for content_fts sync
CREATE TRIGGER IF NOT EXISTS content_ai AFTER INSERT ON content_lines BEGIN
    INSERT INTO content_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS content_ad AFTER DELETE ON content_lines BEGIN
    INSERT INTO content_fts(content_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
"#;

pub const SEARCH_SYMBOLS_QUERY: &str = r#"
SELECT s.name, s.kind, s.container, s.line, s.col, f.path, bm25(symbols_fts, 10.0, 1.0, 5.0) as score
FROM symbols_fts
JOIN symbols s ON symbols_fts.rowid = s.id
JOIN files f ON s.file_id = f.id
WHERE symbols_fts MATCH ?1
ORDER BY score
LIMIT ?2
"#;

pub const SEARCH_CONTENT_QUERY: &str = r#"
SELECT c.content, c.line_num, f.path, f.language, bm25(content_fts) as score
FROM content_fts
JOIN content_lines c ON content_fts.rowid = c.id
JOIN files f ON c.file_id = f.id
WHERE content_fts MATCH ?1
ORDER BY score
LIMIT ?2
"#;

pub const SEARCH_CONTENT_WITH_LANG_QUERY: &str = r#"
SELECT c.content, c.line_num, f.path, f.language, bm25(content_fts) as score
FROM content_fts
JOIN content_lines c ON content_fts.rowid = c.id
JOIN files f ON c.file_id = f.id
WHERE content_fts MATCH ?1 AND f.language = ?2
ORDER BY score
LIMIT ?3
"#;
