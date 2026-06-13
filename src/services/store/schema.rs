pub const SCHEMA_VERSION: i32 = 4;

pub const INIT_SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA cache_size = -16384;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 134217728;
PRAGMA busy_timeout = 5000;
PRAGMA user_version = 4;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    content_hash INTEGER NOT NULL,
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

-- FTS5 trigram index over content_lines, as an EXTERNAL-CONTENT table: it
-- stores no copy of the text, deriving everything from content_lines via the
-- triggers below. The trigram tokenizer is the only FTS5 tokenizer that
-- preserves arbitrary infix substrings (for queries >= 3 chars), so a MATCH is
-- a true necessary condition for the LIKE '%q%' the search still applies as the
-- authority. case_sensitive 0 mirrors LIKE's ASCII case-insensitivity;
-- remove_diacritics 0 keeps accents so folding matches LIKE.
CREATE VIRTUAL TABLE IF NOT EXISTS content_lines_fts USING fts5(
    content,
    content='content_lines',
    content_rowid='id',
    tokenize='trigram case_sensitive 0 remove_diacritics 0'
);

-- Keep the FTS index in lockstep with content_lines through every mutation
-- site automatically — including the two bulk paths (cleanup_expired, clear)
-- that bypass the per-file delete helper — so the index can never desync.
CREATE TRIGGER IF NOT EXISTS content_lines_ai AFTER INSERT ON content_lines BEGIN
    INSERT INTO content_lines_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS content_lines_ad AFTER DELETE ON content_lines BEGIN
    INSERT INTO content_lines_fts(content_lines_fts, rowid, content)
        VALUES ('delete', old.id, old.content);
END;
CREATE TRIGGER IF NOT EXISTS content_lines_au AFTER UPDATE ON content_lines BEGIN
    INSERT INTO content_lines_fts(content_lines_fts, rowid, content)
        VALUES ('delete', old.id, old.content);
    INSERT INTO content_lines_fts(rowid, content) VALUES (new.id, new.content);
END;
"#;

pub fn build_symbol_search_query(with_kind: bool) -> String {
    let kind_filter = if with_kind { " AND s.kind = ?3" } else { "" };
    // `COUNT(*) OVER ()` yields the total match count in the same scan the
    // ORDER BY already pays for, so `count` in list output is exact rather
    // than a limit-saturation guess.
    // Relevance ladder: exact (leaf or full path) > path suffix > name
    // prefix > substring. Exact-leaf is checked first so a nested symbol
    // whose name equals the query still scores 1.0 (a path-suffix test
    // would otherwise shadow it). Length is the ORDER BY tiebreaker, not a
    // score input.
    format!(
        r#"SELECT s.name, s.name_path, s.kind, s.line, s.col, f.path, s.container,
    COUNT(*) OVER () AS total,
    CASE
        WHEN LOWER(s.name) = LOWER(?1) THEN 1.0
        WHEN s.name_path IS NOT NULL AND LOWER(s.name_path) = LOWER(?1) THEN 1.0
        WHEN s.name_path IS NOT NULL AND s.name_path LIKE '%/' || ?1 COLLATE NOCASE THEN 0.9
        WHEN s.name LIKE ?1 || '%' COLLATE NOCASE THEN 0.8
        WHEN s.name LIKE '%' || ?1 || '%' COLLATE NOCASE THEN 0.6
        WHEN s.name_path IS NOT NULL AND s.name_path LIKE '%' || ?1 || '%' COLLATE NOCASE THEN 0.6
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

pub fn build_content_search_query(with_lang: bool, use_fts: bool) -> String {
    let lang_filter = if with_lang {
        " AND f.language = ?3"
    } else {
        ""
    };
    // For queries >= 3 chars, an FTS5 trigram MATCH pre-filters candidate rows
    // sub-linearly. It is ONLY a pre-filter: the LIKE below stays authoritative,
    // so a trigram coincidence that is not a real substring is still rejected,
    // and the result SET, the relevance ladder, and COUNT(*) OVER() are
    // byte-identical to the LIKE-only scan (proven by a set-identity test). The
    // query text is wrapped as a single FTS5 string literal — doubling any
    // embedded quote — so FTS syntax characters in user input never error or
    // change the match. Shorter queries have no trigrams and skip FTS.
    let fts_prefilter = if use_fts {
        r#"c.id IN (SELECT rowid FROM content_lines_fts WHERE content_lines_fts MATCH '"' || REPLACE(?1, '"', '""') || '"') AND "#
    } else {
        ""
    };
    // Relevance is the match's position within the trimmed line — an
    // earlier hit is more relevant. Line length is the ORDER BY tiebreaker
    // only; it carries no relevance signal and must not enter the score.
    format!(
        r#"SELECT c.content, c.line_num, f.path, f.language,
    COUNT(*) OVER () AS total,
    CASE
        WHEN INSTR(TRIM(LOWER(c.content)), LOWER(?1)) = 1 THEN 1.0
        WHEN INSTR(TRIM(LOWER(c.content)), LOWER(?1)) BETWEEN 2 AND 8 THEN 0.8
        WHEN INSTR(TRIM(LOWER(c.content)), LOWER(?1)) BETWEEN 9 AND 32 THEN 0.6
        ELSE 0.4
    END AS score
FROM content_lines c
JOIN files f ON c.file_id = f.id
WHERE {fts_prefilter}c.content LIKE '%' || ?1 || '%' COLLATE NOCASE{lang_filter}
ORDER BY score DESC, LENGTH(c.content) ASC
LIMIT ?2"#
    )
}

/// Minimum query length for the FTS5 trigram pre-filter. A trigram index has
/// no tokens for inputs shorter than 3 characters, so a MATCH would return the
/// empty set; such queries run the LIKE-only scan instead.
pub const FTS_MIN_QUERY_CHARS: usize = 3;
