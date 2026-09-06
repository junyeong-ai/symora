pub const SCHEMA_VERSION: i32 = 7;

pub const INIT_SCHEMA: &str = r#"
-- Incremental auto-vacuum, set before the file is a WAL database and before
-- it holds a table, which are the only moments SQLite lets it be chosen. It
-- is what allows a rebuild to hand the pages it freed back to the filesystem
-- at a point of its choosing; without it those pages stay allocated to the
-- file for good, and an index rebuilt often grows without holding more.
-- Incremental rather than FULL so the relocation is one step after a build
-- rather than work charged to every commit during one.
PRAGMA auto_vacuum = INCREMENTAL;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA cache_size = -16384;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 134217728;
PRAGMA user_version = 7;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- The paths the last completed build could not read — a file it could not
-- open, or a directory it could not enter. They are what the build did not
-- see, so an answer drawn from it is short by whatever they hold. Rows, not a
-- count, because a count cannot be repaired one path at a time, cannot say
-- which language it kept out, and cannot tell a reader where to look.
--
-- `is_file` is what the walk knew, not what a later stat could re-derive: a
-- file's name settles its language and a directory's does not, and a directory
-- named `generated.py` would otherwise be read as holding only Python.
CREATE TABLE IF NOT EXISTS unread_paths (
    path TEXT PRIMARY KEY,
    is_file INTEGER NOT NULL
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
-- preserves arbitrary infix substrings (for queries >= 3 chars), so a trigram
-- MATCH is a necessary condition for `content LIKE '%q%'`. The LIKE filter still
-- runs as the authority, so MATCH only narrows the candidate set and never
-- changes result membership. case_sensitive 0 mirrors LIKE's ASCII case-insensitivity;
-- remove_diacritics 0 keeps accents so folding matches LIKE.
CREATE VIRTUAL TABLE IF NOT EXISTS content_lines_fts USING fts5(
    content,
    content='content_lines',
    content_rowid='id',
    tokenize='trigram case_sensitive 0 remove_diacritics 0'
);

-- Keep the FTS index in lockstep with content_lines through every mutation
-- site automatically — including the bulk path (clear) that bypasses the
-- per-file delete helper — so the index can never desync.
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

/// `lang_count` is the number of language parameters bound after the query
/// (?1), the limit (?2), and the optional kind — the search's domain,
/// restricting rows to those languages. Zero means no language restriction
/// (the whole table).
pub fn build_symbol_search_query(with_kind: bool, lang_count: usize) -> String {
    let kind_filter = if with_kind { " AND s.kind = ?3" } else { "" };
    // Bind order is query(?1), limit(?2), the optional kind, then one slot
    // per covered language, so the first language slot is ?4 when a kind
    // filter precedes it and ?3 otherwise.
    let first_lang_slot = if with_kind { 4 } else { 3 };
    let lang_filter = if lang_count > 0 {
        let slots: Vec<String> = (0..lang_count)
            .map(|i| format!("?{}", i + first_lang_slot))
            .collect();
        format!(" AND f.language IN ({})", slots.join(", "))
    } else {
        String::new()
    };
    // Substring matching is for a LITERAL query: `_`/`%` in an identifier are
    // content, not LIKE wildcards. Escape them (and the escape char) so each
    // `ESCAPE '\'` LIKE matches ?1 verbatim; the exact-match ladder rungs use
    // `=` and need no escaping.
    let like_q = r#"REPLACE(REPLACE(REPLACE(?1, '\', '\\'), '_', '\_'), '%', '\%')"#;
    // `COUNT(*) OVER ()` yields the total match count in the same scan the
    // ORDER BY already pays for, so `count` in list output is exact rather
    // than a limit-saturation guess.
    // Relevance ladder: exact (leaf or full path) > path suffix > name
    // prefix > substring. Exact-leaf is checked first so a nested symbol
    // whose name equals the query still scores 1.0 (a path-suffix test
    // would otherwise shadow it). Length is the ORDER BY tiebreaker, not a
    // score input; path and position close it to a total order (the table is
    // UNIQUE over file/line/col), so which equal-score rows survive the LIMIT
    // is a property of the data, not of the physical row order a rebuild or
    // query plan happens to produce.
    format!(
        r#"SELECT s.name, s.name_path, s.kind, s.line, s.col, f.path, s.container,
    COUNT(*) OVER () AS total,
    CASE
        WHEN LOWER(s.name) = LOWER(?1) THEN 1.0
        WHEN s.name_path IS NOT NULL AND LOWER(s.name_path) = LOWER(?1) THEN 1.0
        WHEN s.name_path IS NOT NULL AND s.name_path LIKE '%/' || {like_q} ESCAPE '\' COLLATE NOCASE THEN 0.9
        WHEN s.name LIKE {like_q} || '%' ESCAPE '\' COLLATE NOCASE THEN 0.8
        WHEN s.name LIKE '%' || {like_q} || '%' ESCAPE '\' COLLATE NOCASE THEN 0.6
        WHEN s.name_path IS NOT NULL AND s.name_path LIKE '%' || {like_q} || '%' ESCAPE '\' COLLATE NOCASE THEN 0.6
        ELSE 0.5
    END AS score
FROM symbols s
JOIN files f ON s.file_id = f.id
WHERE (
    s.name LIKE '%' || {like_q} || '%' ESCAPE '\' COLLATE NOCASE
    OR (s.name_path IS NOT NULL AND s.name_path LIKE '%' || {like_q} || '%' ESCAPE '\' COLLATE NOCASE)
){kind_filter}{lang_filter}
ORDER BY score DESC, LENGTH(COALESCE(s.name_path, s.name)) ASC, f.path ASC, s.line ASC, s.col ASC
LIMIT ?2"#
    )
}

/// `lang_count` is the number of language parameters bound after the query
/// (?1) and limit (?2) — the search's domain, restricting rows to those
/// languages. Zero means no language restriction (the whole table).
pub fn build_content_search_query(lang_count: usize, use_fts: bool) -> String {
    let lang_filter = if lang_count > 0 {
        let slots: Vec<String> = (0..lang_count).map(|i| format!("?{}", i + 3)).collect();
        format!(" AND f.language IN ({})", slots.join(", "))
    } else {
        String::new()
    };
    // For queries >= 3 chars, an FTS5 trigram MATCH pre-filters candidate rows
    // sub-linearly. It is ONLY a pre-filter: the LIKE below stays authoritative,
    // so a trigram coincidence that is not a real substring is still rejected,
    // and the result SET, the relevance ladder, and COUNT(*) OVER() are
    // byte-identical to the LIKE-only scan (proven by a set-identity test). The
    // LIKE is a LITERAL substring test (see `like_q`), so the literal trigram
    // MATCH is a genuine necessary condition for it; the two paths cannot
    // disagree. The query text is wrapped as a single FTS5 string literal —
    // doubling any embedded quote — so FTS syntax characters in user input never
    // error or change the match. Shorter queries have no trigrams and skip FTS.
    let fts_prefilter = if use_fts {
        r#"c.id IN (SELECT rowid FROM content_lines_fts WHERE content_lines_fts MATCH '"' || REPLACE(?1, '"', '""') || '"') AND "#
    } else {
        ""
    };
    // The search is for a LITERAL substring: `_` and `%` are content (snake_case
    // identifiers, format strings), not LIKE wildcards. Escape them — and the
    // escape char itself — so an `ESCAPE '\'` LIKE matches ?1 verbatim. INSTR
    // (scoring) and the trigram MATCH are already literal, so this aligns the
    // LIKE with both.
    let like_q = r#"REPLACE(REPLACE(REPLACE(?1, '\', '\\'), '_', '\_'), '%', '\%')"#;
    // Relevance is the match's position within the trimmed line — an
    // earlier hit is more relevant. Trimming strips tabs as well as spaces
    // (char(9,32)), the same two the scan path strips
    // (`score_content_line`'s `trim_matches([' ', '\t'])`), so a tab-indented
    // line scores the same whichever source answered. Line length is the
    // ORDER BY tiebreaker only; it carries no relevance signal and must not
    // enter the score.
    // Path and line number close the ordering to a total one, so the LIMIT
    // keeps the same rows on both the FTS and LIKE-only plans — without it
    // the two plans retrieve ties in different physical orders and could
    // emit different subsets of the identical result set.
    format!(
        r#"SELECT c.content, c.line_num, f.path, f.language,
    COUNT(*) OVER () AS total,
    CASE
        WHEN INSTR(TRIM(LOWER(c.content), char(9,32)), LOWER(?1)) = 1 THEN 1.0
        WHEN INSTR(TRIM(LOWER(c.content), char(9,32)), LOWER(?1)) BETWEEN 2 AND 8 THEN 0.8
        WHEN INSTR(TRIM(LOWER(c.content), char(9,32)), LOWER(?1)) BETWEEN 9 AND 32 THEN 0.6
        ELSE 0.4
    END AS score
FROM content_lines c
JOIN files f ON c.file_id = f.id
WHERE {fts_prefilter}c.content LIKE '%' || {like_q} || '%' ESCAPE '\' COLLATE NOCASE{lang_filter}
ORDER BY score DESC, LENGTH(c.content) ASC, f.path ASC, c.line_num ASC
LIMIT ?2"#
    )
}

/// Minimum query length for the FTS5 trigram pre-filter. A trigram index has
/// no tokens for inputs shorter than 3 characters, so a MATCH would return the
/// empty set; such queries run the LIKE-only scan instead.
pub const FTS_MIN_QUERY_CHARS: usize = 3;
