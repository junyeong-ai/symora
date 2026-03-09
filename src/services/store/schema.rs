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
    name_path TEXT,
    kind TEXT NOT NULL,
    container TEXT,
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
CREATE INDEX IF NOT EXISTS idx_symbols_name_nocase ON symbols(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_symbols_name_path_nocase ON symbols(name_path COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_content_file ON content_lines(file_id);
"#;

pub const SYMBOL_MIGRATIONS: &[&str] = &[
    "ALTER TABLE symbols ADD COLUMN name_path TEXT",
    "ALTER TABLE symbols ADD COLUMN container TEXT",
    "CREATE INDEX IF NOT EXISTS idx_symbols_name_path_nocase ON symbols(name_path COLLATE NOCASE)",
];

pub fn build_symbol_search_query(with_kind: bool) -> String {
    let kind_filter = if with_kind { " AND s.kind = ?3" } else { "" };
    format!(
        r#"SELECT s.name, s.name_path, s.kind, s.line, s.col, f.path, s.container,
    CASE
        WHEN s.name_path IS NOT NULL AND LOWER(s.name_path) = LOWER(?1) THEN 1.0
        WHEN s.name_path IS NOT NULL AND s.name_path LIKE '%/' || ?1 COLLATE NOCASE THEN 0.96
        WHEN LOWER(s.name) = LOWER(?1) THEN 1.0
        WHEN s.name LIKE ?1 || '%' COLLATE NOCASE THEN 0.9
        WHEN s.name LIKE '%' || ?1 COLLATE NOCASE THEN 0.7
        WHEN s.name_path IS NOT NULL AND s.name_path LIKE '%' || ?1 || '%' COLLATE NOCASE THEN 0.68
        ELSE 0.5
    END AS score
FROM symbols s
JOIN files f ON s.file_id = f.id
WHERE (
    s.name LIKE '%' || ?1 || '%' COLLATE NOCASE
    OR (s.name_path IS NOT NULL AND s.name_path LIKE '%' || ?1 || '%' COLLATE NOCASE)
){kind_filter}
ORDER BY score DESC, LENGTH(COALESCE(s.name_path, s.name)) ASC
LIMIT ?2"#
    )
}

pub fn build_content_search_query(with_lang: bool) -> String {
    let lang_filter = if with_lang {
        " AND f.language = ?3"
    } else {
        ""
    };
    format!(
        r#"SELECT c.content, c.line_num, f.path, f.language,
    CASE
        WHEN INSTR(TRIM(LOWER(c.content)), LOWER(?1)) = 1 THEN 1.0
        WHEN LENGTH(c.content) < 80 THEN 0.85
        WHEN INSTR(LOWER(c.content), LOWER(?1)) <= 20 THEN 0.7
        WHEN LENGTH(c.content) < 150 THEN 0.5
        ELSE 0.3
    END AS score
FROM content_lines c
JOIN files f ON c.file_id = f.id
WHERE c.content LIKE '%' || ?1 || '%' COLLATE NOCASE{lang_filter}
ORDER BY score DESC, LENGTH(c.content) ASC
LIMIT ?2"#
    )
}
