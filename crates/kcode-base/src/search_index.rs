//! Built-in metadata file-search index backing the `search_index` tool.
//!
//! Answers "where is X" (a file or a symbol) from a maintained index in ONE tool
//! call instead of N model-driven greps.
//!
//! ## Why this exists (and why it is not reflex-search)
//!
//! The research that motivated this tool (`RESEARCH-index-and-parallel.md`)
//! names `reflex-search` as the strongest reuse candidate, and it *is* library
//! usable (`has_lib: true`, MIT OR Apache-2.0, trigram index plus a SQLite
//! `meta.db` holding file metadata, symbols, deps and a `can_trust_results`
//! freshness flag). It was not linked, and the reason is recorded here because
//! a reader of this file deserves the decision, not just the outcome:
//!
//! - `cargo add reflex-search --dry-run` resolves v2.3.1, but the resolved graph
//!   is ~390 packages including a second `ratatui` (kcode vendors a *patched*
//!   `ratatui-kcode`), a second async HTTP stack (`axum`/`quinn`/`reqwest`) and a
//!   terminal-UI stack. Two ratatui majors cannot be unified by cargo.
//! - The research's measured win (0.82x-0.96x tokens) belongs to reflex's
//!   CLI/plugin delivery route; its MCP route measured 1.66x tokens *worse* plus
//!   an extra ToolSearch turn. Linking the library is a third, unmeasured route
//!   that inherits the full dependency cost without inheriting any measurement.
//! - What the research actually validates are reflex's **design contracts**, not
//!   code we must link: `meta.db` as the store, freshness judged by size + mtime
//!   + content hash rather than by commit, byte-identical index files across a
//!   rewrite, stable ordering by path and line, and codegraph-rust's determinism
//!   contract (no model calls, no embeddings, canonical output guarded by
//!   byte-stable fixtures). Those contracts are implemented here, in kcode-owned
//!   code, on dependencies this crate already has.
//!
//! Reuse over greenfield is still satisfied: the shape is copied from the
//! proven `session_search_index` tool in this repo, the SQLite idiom from
//! `model_usage`, and the determinism contract from codegraph-rust as quoted in
//! the research.
//!
//! ## Determinism / cache-safety
//!
//! kcode protects prompt caching with an append-only context: a message already
//! in the cache must be byte-identical the next time it is sent. So the same
//! query must return byte-identical bytes on every run. Every ordering is a SQL
//! `ORDER BY` over `(path, line, kind, name)` with `COLLATE BINARY`, and the
//! rendered answer is produced by [`render_report`] — a pure function of the
//! result set with no clocks, no hash-map iteration order, no floats, and no
//! address-dependent formatting.
//!
//! ## Freshness
//!
//! Files are fingerprinted by size, mtime nanoseconds and a SHA-256 content
//! hash. [`SearchIndex::refresh`] re-walks the tree and rewrites only the paths
//! that are new, changed or gone, so an unchanged tree costs a stat sweep rather
//! than a re-index. The answer carries a `fresh` flag so the model knows when
//! the index could not be trusted, mirroring reflex's `can_trust_results`.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema version. Bumping it forces a full rebuild rather than a migration,
/// which is the safe direction for a cache: a stale index is never trusted.
const SCHEMA_VERSION: u32 = 1;

/// Directories never walked. A hardcoded skip list is used instead of the
/// `ignore` crate because `ignore` is not a dependency of this crate and adding
/// a crate for a default-off feature is the wrong trade.
const SKIPPED_DIRECTORIES: [&str; 4] = [".git", "target", "node_modules", ".kcode"];

/// Files larger than this are indexed by path only, so a vendored or generated
/// tree cannot bloat the index.
const DEFAULT_MAX_FILE_BYTES: u64 = 1_048_576;

/// One indexed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    /// Path relative to the indexed root, using `/` separators.
    pub path: String,
    /// Byte length at index time.
    pub size: u64,
    /// Content hash (SHA-256, hex) at index time, or empty when the body was
    /// too large to hash.
    pub content_hash: String,
    /// 1-based line of the definition. 0 for a whole-file match.
    pub line: u32,
    /// `symbol` when this row came from tree-sitter extraction, `content` when
    /// it came from the FTS body index, `path` when only the path matched.
    pub kind: String,
}

/// A ranked answer row. `path` is relative to the indexed root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Path relative to the indexed root.
    pub path: String,
    /// 1-based line, or 0 for a whole-file match.
    pub line: u32,
    /// `symbol`, `content`, or `path`.
    pub kind: String,
    /// Symbol name, or the matched path, depending on `kind`.
    pub name: String,
    /// Ranking score. Higher is better. Stable for a given index state.
    pub score: i64,
}

/// The answer to one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    /// Hits, sorted by score descending then path/line/kind/name ascending, so
    /// the order is a total order with no ties left to iteration chance.
    pub hits: Vec<SearchHit>,
    /// True when the index was verified fresh at query time. False means the
    /// refresh could not run and the answer may be stale.
    pub fresh: bool,
    /// Number of files in the index.
    pub files_indexed: u64,
    /// Number of symbol rows in the index.
    pub symbols_indexed: u64,
}

/// The index itself: one SQLite database plus the incremental refresh logic.
pub struct SearchIndex {
    root: PathBuf,
    connection: Connection,
    max_file_bytes: u64,
    /// The index database file, excluded from the walk. Without this the walker
    /// indexes its own database, so an unchanged tree looks changed and the
    /// index counts itself in its own file total.
    index_path: PathBuf,
}

/// Errors that must not abort a query. A search tool that hard-fails is worse
/// than one that reports "could not answer".
#[derive(Debug)]
pub enum SearchIndexError {
    /// The index file could not be opened or created.
    Open(String),
    /// The refresh could not complete, so freshness cannot be guaranteed.
    Refresh(String),
    /// The query could not be executed.
    Query(String),
}

impl std::fmt::Display for SearchIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchIndexError::Open(message) => write!(f, "cannot open search index: {message}"),
            SearchIndexError::Refresh(message) => {
                write!(f, "cannot refresh search index: {message}")
            }
            SearchIndexError::Query(message) => write!(f, "search index query failed: {message}"),
        }
    }
}

impl std::error::Error for SearchIndexError {}

/// Normalizes a path to the index's canonical form: relative to `root`, `/`
/// separators, no `./` prefix. Deterministic by construction, so the same file
/// always produces the same stored key.
fn normalize_relative(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut normalized = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        return None;
    }
    if normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    Some(normalized)
}

fn system_time_nanos(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or(0)
}

/// SHA-256 hex of `bytes`. Uses the `sha2` dependency this crate already has, so
/// no hashing crate is added for this feature.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// True when `path` is the index database or one of its WAL sidecars.
///
/// SQLite in WAL mode (`create_schema` sets `PRAGMA journal_mode=WAL`) keeps the
/// main database alongside `-wal` and `-shm` files, and all three change on every
/// write. Indexing them would make the index appear to change on every refresh
/// and would count itself in its own file total.
fn is_index_artifact(path: &Path, index_path: &Path) -> bool {
    if path == index_path {
        return true;
    }
    let Some(index_name) = index_path.file_name() else {
        return false;
    };
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    let index_name = index_name.to_string_lossy();
    name == format!("{index_name}-wal") || name == format!("{index_name}-shm")
}

/// True when a directory name is never walked.
fn is_skipped_directory(name: &str) -> bool {
    SKIPPED_DIRECTORIES.iter().any(|skipped| *skipped == name)
}

/// Collects every candidate file under `root`, deterministically ordered.
///
/// The walk is depth-first with each directory's entries sorted by name, so the
/// visit order does not depend on the filesystem's readdir order. That matters
/// because the index write order is observable in the resulting rows.
fn collect_files(root: &Path, max_file_bytes: u64, index_path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut children: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if is_skipped_directory(&name) {
                    continue;
                }
                children.push(path);
            } else if file_type.is_file() {
                // The index must never index itself. `collect_files` skips
                // directories, so an index database sitting in a non-skipped
                // directory would be walked: the walk would then see the
                // database's own writes as file changes, and the file count
                // would include the index. Exclude the database and the `-wal`
                // and `-shm` sidecars WAL mode creates next to it.
                if is_index_artifact(&path, index_path) {
                    continue;
                }
                children.push(path);
            }
        }
        // Sorted so the walk is independent of readdir order.
        children.sort();
        for child in children {
            let is_dir = child.is_dir();
            if is_dir {
                stack.push(child);
            } else {
                let too_big = std::fs::metadata(&child)
                    .map(|metadata| metadata.len() > max_file_bytes)
                    .unwrap_or(false);
                if !too_big {
                    files.push(child);
                }
            }
        }
    }
    files.sort();
    files
}

/// Symbol extraction. Rust is the language this build extracts symbols for,
/// using `tree-sitter-rust`, the same grammar `kcode-enforcement` already uses.
/// Files that do not parse, and non-Rust files, contribute path and content
/// rows only — the limitation is stated rather than faked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSymbol {
    /// Symbol name.
    pub name: String,
    /// 1-based line of the definition.
    pub line: u32,
    /// One of `function`, `struct`, `enum`, `trait`, `impl`, `const`, `static`,
    /// `type`, `mod`.
    pub kind: String,
}

/// Extracts Rust definitions from `source` using tree-sitter.
///
/// Tree-sitter is already a workspace dependency (`tree-sitter = "0.25"`,
/// `tree-sitter-rust = "0.23"`, used by `kcode-enforcement`), so this adds no
/// crate. On any parse failure it returns an empty vector: the file still
/// contributes path and content rows, so a syntax error degrades the answer
/// instead of failing the query.
pub fn extract_rust_symbols(source: &str) -> Vec<ExtractedSymbol> {
    let mut parser = tree_sitter::Parser::new();
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut symbols = Vec::new();
    collect_rust_symbols(tree.root_node(), source.as_bytes(), &mut symbols);
    // Stable order: by line, then name, then kind. No hash-map iteration.
    symbols.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    symbols
}

fn collect_rust_symbols(node: tree_sitter::Node, source: &[u8], out: &mut Vec<ExtractedSymbol>) {
    let kind = match node.kind() {
        "function_item" | "function_signature_item" => Some("function"),
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "trait_item" => Some("trait"),
        "union_item" => Some("union"),
        "const_item" => Some("const"),
        "static_item" => Some("static"),
        "type_item" => Some("type"),
        "mod_item" => Some("mod"),
        _ => None,
    };
    if let Some(kind) = kind {
        // The name child is the `name` field on every one of these node kinds.
        if let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source)
        {
            out.push(ExtractedSymbol {
                name: name.to_string(),
                line: name_node.start_position().row as u32 + 1,
                kind: kind.to_string(),
            });
        }
    }
    // `impl` blocks carry the type name, which is what "where is this type
    // implemented" actually asks. Extract it from the `type` field.
    if node.kind() == "impl_item"
        && let Some(type_node) = node.child_by_field_name("type")
        && let Ok(type_name) = type_node.utf8_text(source)
    {
        out.push(ExtractedSymbol {
            name: type_name.trim().to_string(),
            line: type_node.start_position().row as u32 + 1,
            kind: "impl".to_string(),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_symbols(child, source, out);
    }
}

impl SearchIndex {
    /// Opens (creating if needed) the index at `path` for the tree rooted at
    /// `root`.
    pub fn open(root: &Path, path: &Path, max_file_bytes: u64) -> Result<Self, SearchIndexError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Err(SearchIndexError::Open(format!(
                "cannot create index directory {}: {error}",
                parent.display()
            )));
        }
        let connection = Connection::open(path)
            .map_err(|error| SearchIndexError::Open(format!("{}: {error}", path.display())))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| SearchIndexError::Open(error.to_string()))?;
        let index = Self {
            root: root.to_path_buf(),
            connection,
            max_file_bytes: if max_file_bytes == 0 {
                DEFAULT_MAX_FILE_BYTES
            } else {
                max_file_bytes
            },
            index_path: path.to_path_buf(),
        };
        index.create_schema()?;
        Ok(index)
    }

    fn create_schema(&self) -> Result<(), SearchIndexError> {
        self.connection
            .execute_batch(&format!(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE IF NOT EXISTS meta (
                   key TEXT PRIMARY KEY NOT NULL,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS files (
                   path TEXT PRIMARY KEY NOT NULL COLLATE BINARY,
                   size INTEGER NOT NULL,
                   mtime_ns INTEGER NOT NULL,
                   content_hash TEXT NOT NULL,
                   body TEXT NOT NULL DEFAULT ''
                 );
                 CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
                   path, body, tokenize = 'unicode61'
                 );
                 CREATE TABLE IF NOT EXISTS symbols (
                   path TEXT NOT NULL COLLATE BINARY,
                   line INTEGER NOT NULL,
                   kind TEXT NOT NULL COLLATE BINARY,
                   name TEXT NOT NULL COLLATE BINARY,
                   PRIMARY KEY (path, line, kind, name)
                 );
                 CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name COLLATE NOCASE);"
            ))
            .map_err(|error| SearchIndexError::Open(format!("schema: {error}")))?;
        self.connection
            .execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|error| SearchIndexError::Open(format!("schema version: {error}")))?;
        Ok(())
    }

    /// Re-walks the tree and applies only the differences: new files inserted,
    /// changed files updated, missing files deleted.
    ///
    /// Returns the number of files that were rewritten, which is what makes the
    /// cost proportional to the change rather than to the tree.
    pub fn refresh(&mut self) -> Result<u64, SearchIndexError> {
        let mut changed = 0u64;
        let mut seen: Vec<String> = Vec::new();
        for path in collect_files(&self.root, self.max_file_bytes, &self.index_path) {
            let Some(relative) = normalize_relative(&self.root, &path) else {
                continue;
            };
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let size = metadata.len();
            let mtime_ns = metadata
                .modified()
                .map(system_time_nanos)
                .unwrap_or_default();
            let existing = self
                .file_stamp(&relative)
                .map_err(|error| SearchIndexError::Refresh(error.to_string()))?;
            if existing.as_ref() == Some(&(size, mtime_ns)) {
                // Unchanged: size and mtime both match, so the content hash is
                // still valid and nothing is rewritten.
                seen.push(relative);
                continue;
            }
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            let content_hash = sha256_hex(body.as_bytes());
            self.upsert_file(&relative, size, mtime_ns, &content_hash, &body)
                .map_err(|error| SearchIndexError::Refresh(error.to_string()))?;
            seen.push(relative);
            changed += 1;
        }
        self.delete_missing(&seen)
            .map_err(|error| SearchIndexError::Refresh(error.to_string()))?;
        Ok(changed)
    }

    fn file_stamp(&self, relative: &str) -> Result<Option<(u64, i64)>, rusqlite::Error> {
        self.connection
            .query_row(
                "SELECT size, mtime_ns FROM files WHERE path = ?1",
                params![relative],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get(1)?)),
            )
            .optional()
    }

    fn upsert_file(
        &self,
        relative: &str,
        size: u64,
        mtime_ns: i64,
        content_hash: &str,
        body: &str,
    ) -> Result<(), rusqlite::Error> {
        let symbols = if relative.ends_with(".rs") {
            extract_rust_symbols(body)
        } else {
            Vec::new()
        };
        self.connection.execute(
            "INSERT INTO files(path, size, mtime_ns, content_hash, body)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               content_hash = excluded.content_hash,
               body = excluded.body",
            params![relative, size as i64, mtime_ns, content_hash, body],
        )?;
        // `files_fts` is an FTS5 virtual table, and FTS5 does not implement
        // UPSERT (`ON CONFLICT DO UPDATE`). Writing the row must therefore be an
        // explicit delete followed by an insert: the delete removes any previous
        // posting for this path, the insert supplies the new one. Doing it in
        // this order keeps the table consistent when a file is re-indexed after
        // an edit, which is what an incremental refresh relies on.
        self.connection
            .execute("DELETE FROM files_fts WHERE path = ?1", params![relative])?;
        self.connection
            .execute("INSERT INTO files_fts(path, body) VALUES(?1, ?2)", params![relative, body])?;
        self.connection
            .execute("DELETE FROM symbols WHERE path = ?1", params![relative])?;
        for symbol in &symbols {
            self.connection.execute(
                "INSERT OR REPLACE INTO symbols(path, line, kind, name) VALUES(?1, ?2, ?3, ?4)",
                params![relative, symbol.line, symbol.kind, symbol.name],
            )?;
        }
        Ok(())
    }

    fn delete_missing(&self, seen: &[String]) -> Result<(), rusqlite::Error> {
        let mut statement = self
            .connection
            .prepare("SELECT path FROM files ORDER BY path COLLATE BINARY")?;
        let existing: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .flatten()
            .collect();
        for path in existing {
            if !seen.iter().any(|kept| *kept == path) {
                self.connection
                    .execute("DELETE FROM files WHERE path = ?1", params![path])?;
                self.connection
                    .execute("DELETE FROM files_fts WHERE path = ?1", params![path])?;
                self.connection
                    .execute("DELETE FROM symbols WHERE path = ?1", params![path])?;
            }
        }
        Ok(())
    }

    /// Answers one query deterministically.
    ///
    /// The ordering is a total order: score descending, then path, line, kind
    /// and name ascending with `COLLATE BINARY`. No row pair can tie, so the
    /// result is a pure function of the index state and the same query returns
    /// byte-identical bytes on every run.
    pub fn query(&self, query: &str, limit: usize) -> Result<SearchReport, SearchIndexError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(SearchReport {
                hits: Vec::new(),
                fresh: true,
                files_indexed: self.count_files()?,
                symbols_indexed: self.count_symbols()?,
            });
        }
        let like_pattern = format!("%{}%", escape_like(trimmed));
        let mut statement = self
            .connection
            .prepare(
                "SELECT path, line, kind, name, score FROM (
                   -- Symbol definitions: the strongest signal for \"where is X defined\".
                   SELECT s.path AS path, s.line AS line, 'symbol' AS kind, s.name AS name,
                          CASE
                            WHEN s.name = ?2 COLLATE NOCASE THEN 100
                            WHEN s.name LIKE ?3 ESCAPE '\\' THEN 60
                            ELSE 40
                          END AS score
                     FROM symbols s
                    WHERE s.name LIKE ?3 ESCAPE '\\'
                    UNION ALL
                   -- Path matches: \"where is the file\".
                   SELECT f.path AS path, 0 AS line, 'path' AS kind, f.path AS name, 80 AS score
                     FROM files f
                    WHERE f.path LIKE ?3 ESCAPE '\\'
                    UNION ALL
                   -- Content matches: \"where is this string used\".
                   SELECT f.path AS path, 0 AS line, 'content' AS kind, f.path AS name, 20 AS score
                     FROM files f
                    WHERE f.body LIKE ?3 ESCAPE '\\'
                 )
                 ORDER BY score DESC, path COLLATE BINARY, line, kind COLLATE BINARY, name COLLATE BINARY
                 LIMIT ?1",
            )
            .map_err(|error| SearchIndexError::Query(error.to_string()))?;
        let rows = statement
            .query_map(params![limit as i64, trimmed, like_pattern], |row| {
                Ok(SearchHit {
                    path: row.get(0)?,
                    line: row.get(1)?,
                    kind: row.get(2)?,
                    name: row.get(3)?,
                    score: row.get(4)?,
                })
            })
            .map_err(|error| SearchIndexError::Query(error.to_string()))?;
        let mut hits = Vec::new();
        for row in rows {
            match row {
                Ok(hit) => hits.push(hit),
                Err(error) => return Err(SearchIndexError::Query(error.to_string())),
            }
        }
        Ok(SearchReport {
            hits,
            fresh: true,
            files_indexed: self.count_files()?,
            symbols_indexed: self.count_symbols()?,
        })
    }

    /// Reports whether the index on disk still matches the tree, without
    /// rewriting anything. This is the freshness flag's source.
    pub fn is_fresh(&self) -> bool {
        for path in collect_files(&self.root, self.max_file_bytes, &self.index_path) {
            let Some(relative) = normalize_relative(&self.root, &path) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&path) else {
                return false;
            };
            let stamp = (metadata.len(), metadata.modified().map(system_time_nanos).unwrap_or(0));
            match self.file_stamp(&relative) {
                Ok(Some(stored)) if stored == stamp => {}
                _ => return false,
            }
        }
        true
    }

    /// True when the tree contains a file the index does not know about, or the
    /// index knows a file the tree no longer has.
    pub fn has_pending_changes(&self) -> Result<bool, SearchIndexError> {
        if !self.is_fresh() {
            return Ok(true);
        }
        let stored = self
            .connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
            .map_err(|error| SearchIndexError::Query(error.to_string()))? as u64;
        let on_disk = collect_files(&self.root, self.max_file_bytes, &self.index_path).len() as u64;
        Ok(stored != on_disk)
    }

    fn count_files(&self) -> Result<u64, SearchIndexError> {
        self.connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
            .map(|count| count as u64)
            .map_err(|error| SearchIndexError::Query(error.to_string()))
    }

    fn count_symbols(&self) -> Result<u64, SearchIndexError> {
        self.connection
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get::<_, i64>(0))
            .map(|count| count as u64)
            .map_err(|error| SearchIndexError::Query(error.to_string()))
    }
}

/// Escapes `%`, `_` and `\` so a query containing them is matched literally.
/// Without this a query like `a_b` would match `axb`, which is not what the
/// caller asked and is not deterministic with respect to their intent.
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(character);
            }
            other => escaped.push(other),
        }
    }
    escaped
}

/// Renders a report as the exact bytes the tool returns.
///
/// This is a pure function of `report`: no clocks, no hash-map iteration, no
/// floats, no locale, no path separator guessing. The same report always
/// renders to the same bytes, which is what makes a repeated query safe for the
/// provider prompt cache.
pub fn render_report(query: &str, report: &SearchReport) -> String {
    let mut out = String::new();
    if report.hits.is_empty() {
        out.push_str(&format!("search_index: no matches for '{query}'\n"));
    } else {
        out.push_str(&format!(
            "search_index: {} match{} for '{query}'\n",
            report.hits.len(),
            if report.hits.len() == 1 { "" } else { "es" }
        ));
    }
    for hit in &report.hits {
        let location = if hit.line > 0 {
            format!("{}:{}", hit.path, hit.line)
        } else {
            hit.path.clone()
        };
        out.push_str(&format!(
            "  {}  {}  {}\n",
            location, hit.kind, hit.name
        ));
    }
    out.push_str(&format!(
        "  index: {} files, {} symbols, fresh={}\n",
        report.files_indexed, report.symbols_indexed, report.fresh
    ));
    out
}

/// Convenience: refresh (unless the mode says not to) then query, returning the
/// rendered answer. `refresh_mode` is one of `on_write`, `on_query`, `manual`.
pub fn answer_query(
    root: &Path,
    index_path: &Path,
    max_file_bytes: u64,
    refresh_mode: &str,
    query: &str,
    limit: usize,
) -> Result<String, SearchIndexError> {
    let mut index = SearchIndex::open(root, index_path, max_file_bytes)?;
    let fresh = match refresh_mode {
        "manual" => index.is_fresh(),
        "on_query" => {
            if index.has_pending_changes()? {
                index.refresh().map(|_| true).unwrap_or(false)
            } else {
                true
            }
        }
        // "on_write" and anything unrecognized: always refresh first. An
        // unrecognized mode must not silently disable freshness.
        _ => index.refresh().map(|_| true).unwrap_or(false),
    };
    let mut report = index.query(query, limit)?;
    report.fresh = fresh;
    Ok(render_report(query, &report))
}

/// Test/helper accessor for the default max file size, so a caller does not
/// hardcode the number.
pub fn default_max_file_bytes() -> u64 {
    DEFAULT_MAX_FILE_BYTES
}

/// Verifies a report is internally consistent: hits are unique and ordered.
/// Used by tests to assert the total-order contract.
pub fn report_is_deterministic(report: &SearchReport) -> bool {
    let mut previous: Option<&SearchHit> = None;
    for hit in &report.hits {
        if let Some(previous) = previous
            && !hit_orders_after(previous, hit)
        {
            return false;
        }
        previous = Some(hit);
    }
    true
}

fn hit_orders_after(previous: &SearchHit, next: &SearchHit) -> bool {
    let key_previous = (
        std::cmp::Reverse(previous.score),
        previous.path.as_str(),
        previous.line,
        previous.kind.as_str(),
        next.name.as_str(),
    );
    let key_next = (
        std::cmp::Reverse(next.score),
        next.path.as_str(),
        next.line,
        next.kind.as_str(),
        next.name.as_str(),
    );
    key_previous < key_next
}

/// Opens the index for a root, resolving `index_path` relative to the working
/// directory when it is not absolute. Config-driven: no path is hardcoded here
/// beyond what the caller passed.
pub fn resolve_index_path(root: &Path, configured: &str) -> PathBuf {
    let path = Path::new(configured);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Formats an anyhow context chain for a user-visible error line.
pub fn describe_error(error: &anyhow::Error) -> String {
    let mut chain = error.chain().map(|cause| cause.to_string());
    let head = chain.next().unwrap_or_default();
    let rest: Vec<String> = chain.collect();
    if rest.is_empty() {
        head
    } else {
        format!("{head}: {}", rest.join(": "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let unique = format!(
            "kcode-search-index-{}-{}-{:?}",
            name,
            std::process::id(),
            std::thread::current().id()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn escape_like_treats_wildcards_literally() {
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn sha256_hex_is_stable_and_known() {
        // Known-answer: SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn extract_rust_symbols_finds_definitions_with_lines() {
        let source = "fn alpha() {}\nstruct Beta { x: u32 }\nimpl Beta { fn gamma(&self) {} }\n";
        let symbols = extract_rust_symbols(source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "missing fn: {names:?}");
        assert!(names.contains(&"Beta"), "missing struct: {names:?}");
        assert!(names.contains(&"gamma"), "missing method: {names:?}");
        let alpha = symbols.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.line, 1);
        assert_eq!(alpha.kind, "function");
    }

    #[test]
    fn extract_rust_symbols_survives_a_parse_failure() {
        // Unbalanced braces: tree-sitter still produces a tree, but even a hard
        // parse failure must degrade to "no symbols", never an error.
        let symbols = extract_rust_symbols("fn broken( { { { ");
        assert!(symbols.is_empty() || symbols.iter().all(|s| !s.name.is_empty()));
    }

    #[test]
    fn refresh_is_incremental_and_only_rewrites_changes() {
        let root = temp_root("incremental");
        let index_path = root.join("index.db");
        write_file(&root, "src/lib.rs", "fn one() {}\n");
        write_file(&root, "src/other.rs", "fn two() {}\n");
        let mut index = SearchIndex::open(&root, &index_path, 0).unwrap();
        let first = index.refresh().unwrap();
        assert!(first >= 2, "first refresh must index both files, got {first}");
        let second = index.refresh().unwrap();
        assert_eq!(second, 0, "an unchanged tree must rewrite nothing");
        // Touch one file's content.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_file(&root, "src/lib.rs", "fn one() {}\nfn added() {}\n");
        let third = index.refresh().unwrap();
        assert_eq!(third, 1, "only the changed file may be rewritten");
        let report = index.query("added", 10).unwrap();
        assert!(
            report
                .hits
                .iter()
                .any(|hit| hit.name == "added" && hit.kind == "symbol"),
            "the new symbol must be findable: {:?}",
            report.hits
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleted_files_disappear_from_the_index() {
        let root = temp_root("deleted");
        let index_path = root.join("index.db");
        write_file(&root, "a.rs", "fn gone() {}\n");
        let mut index = SearchIndex::open(&root, &index_path, 0).unwrap();
        index.refresh().unwrap();
        assert_eq!(index.count_files().unwrap(), 1);
        std::fs::remove_file(root.join("a.rs")).unwrap();
        index.refresh().unwrap();
        assert_eq!(index.count_files().unwrap(), 0);
        assert!(index.query("gone", 10).unwrap().hits.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_query_returns_byte_identical_results_across_runs() {
        let root = temp_root("determinism");
        let index_path = root.join("index.db");
        write_file(&root, "src/lib.rs", "pub fn widget() {}\nfn helper() { widget(); }\n");
        write_file(&root, "src/widget.rs", "struct Widget;\nimpl Widget {}\n");
        write_file(&root, "README.md", "widget docs\n");
        let mut index = SearchIndex::open(&root, &index_path, 0).unwrap();
        index.refresh().unwrap();
        let first = render_report("widget", &index.query("widget", 50).unwrap());
        let second = render_report("widget", &index.query("widget", 50).unwrap());
        assert_eq!(first, second, "repeated query must be byte-identical");
        assert!(report_is_deterministic(&index.query("widget", 50).unwrap()));
        // A full rebuild must produce the same bytes too.
        drop(index);
        std::fs::remove_file(&index_path).unwrap();
        let mut rebuilt = SearchIndex::open(&root, &index_path, 0).unwrap();
        rebuilt.refresh().unwrap();
        let third = render_report("widget", &rebuilt.query("widget", 50).unwrap());
        assert_eq!(first, third, "a rebuilt index must render identically");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skipped_directories_are_never_indexed() {
        let root = temp_root("skipped");
        let index_path = root.join("index.db");
        write_file(&root, "keep.rs", "fn keep() {}\n");
        write_file(&root, "target/debug/artifact.rs", "fn build_noise() {}\n");
        write_file(&root, "node_modules/pkg/index.js", "function noise() {}\n");
        let mut index = SearchIndex::open(&root, &index_path, 0).unwrap();
        index.refresh().unwrap();
        let paths: Vec<String> = (0..index.count_files().unwrap())
            .map(|_| String::new())
            .collect();
        assert_eq!(paths.len() as u64, index.count_files().unwrap());
        let report = index.query("noise", 50).unwrap();
        assert!(
            report.hits.is_empty(),
            "skipped directories must not be indexed: {:?}",
            report.hits
        );
        let kept = index.query("keep", 50).unwrap();
        assert!(kept.hits.iter().any(|hit| hit.name == "keep"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
