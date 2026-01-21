pub const INIT_SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    mtime INTEGER NOT NULL DEFAULT 0,
    language TEXT,
    indexed_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id),
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    UNIQUE (file_id, line, col)
);

CREATE TABLE IF NOT EXISTS content_lines (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id),
    line_num INTEGER NOT NULL,
    content TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_content_file ON content_lines(file_id);
"#;

pub const SEARCH_SYMBOLS_QUERY: &str = r#"
SELECT s.name, s.kind, s.line, s.col, f.path,
    CASE
        WHEN LOWER(s.name) = LOWER(?1) THEN 1.0
        WHEN s.name LIKE ?1 || '%' COLLATE NOCASE THEN 0.9
        WHEN s.name LIKE '%' || ?1 COLLATE NOCASE THEN 0.7
        ELSE 0.5
    END AS score
FROM symbols s
JOIN files f ON s.file_id = f.id
WHERE s.name LIKE '%' || ?1 || '%' COLLATE NOCASE
ORDER BY score DESC, LENGTH(s.name) ASC
LIMIT ?2
"#;

pub const SEARCH_SYMBOLS_WITH_KIND_QUERY: &str = r#"
SELECT s.name, s.kind, s.line, s.col, f.path,
    CASE
        WHEN LOWER(s.name) = LOWER(?1) THEN 1.0
        WHEN s.name LIKE ?1 || '%' COLLATE NOCASE THEN 0.9
        WHEN s.name LIKE '%' || ?1 COLLATE NOCASE THEN 0.7
        ELSE 0.5
    END AS score
FROM symbols s
JOIN files f ON s.file_id = f.id
WHERE s.name LIKE '%' || ?1 || '%' COLLATE NOCASE AND s.kind = ?3
ORDER BY score DESC, LENGTH(s.name) ASC
LIMIT ?2
"#;

pub const SEARCH_CONTENT_QUERY: &str = r#"
SELECT c.content, c.line_num, f.path, f.language,
    CASE
        WHEN INSTR(TRIM(LOWER(c.content)), LOWER(?1)) = 1 THEN 1.0
        WHEN LENGTH(c.content) < 80 THEN 0.85
        WHEN INSTR(LOWER(c.content), LOWER(?1)) <= 20 THEN 0.7
        WHEN LENGTH(c.content) < 150 THEN 0.5
        ELSE 0.3
    END AS score
FROM content_lines c
JOIN files f ON c.file_id = f.id
WHERE c.content LIKE '%' || ?1 || '%' COLLATE NOCASE
ORDER BY score DESC, LENGTH(c.content) ASC
LIMIT ?2
"#;

pub const SEARCH_CONTENT_WITH_LANG_QUERY: &str = r#"
SELECT c.content, c.line_num, f.path, f.language,
    CASE
        WHEN INSTR(TRIM(LOWER(c.content)), LOWER(?1)) = 1 THEN 1.0
        WHEN LENGTH(c.content) < 80 THEN 0.85
        WHEN INSTR(LOWER(c.content), LOWER(?1)) <= 20 THEN 0.7
        WHEN LENGTH(c.content) < 150 THEN 0.5
        ELSE 0.3
    END AS score
FROM content_lines c
JOIN files f ON c.file_id = f.id
WHERE c.content LIKE '%' || ?1 || '%' COLLATE NOCASE AND f.language = ?3
ORDER BY score DESC, LENGTH(c.content) ASC
LIMIT ?2
"#;
