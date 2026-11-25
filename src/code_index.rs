//! A symbol index built with tree-sitter: every function, type, method and module
//! definition in the project, for the `symbols`, `find_symbol` and `read_symbol` tools.
//!
//! It lives in SQLite next to the sessions and is brought up to date before each
//! lookup by checking file sizes and times (respecting `.gitignore`), so it follows
//! edits without a file watcher. Languages are Cargo features (`lang-rust`, ...).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, params};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub path: String,
    pub name: String,
    pub kind: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
}

struct Grammar {
    language: Language,
    query: Query,
}

/// The grammar for a file, by extension.
fn grammar(path: &Path) -> Option<&'static Grammar> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let key: Option<&'static str> = match ext.as_str() {
        #[cfg(feature = "lang-rust")]
        "rs" => Some("rust"),
        #[cfg(feature = "lang-typescript")]
        "ts" | "mts" | "cts" => Some("typescript"),
        #[cfg(feature = "lang-typescript")]
        "tsx" => Some("tsx"),
        #[cfg(feature = "lang-javascript")]
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        #[cfg(feature = "lang-python")]
        "py" | "pyi" => Some("python"),
        #[cfg(feature = "lang-go")]
        "go" => Some("go"),
        #[cfg(feature = "lang-c")]
        "c" | "h" => Some("c"),
        #[cfg(feature = "lang-cpp")]
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some("cpp"),
        #[cfg(feature = "lang-java")]
        "java" => Some("java"),
        _ => None,
    };
    let key = key?;
    static GRAMMARS: OnceLock<HashMap<&'static str, Grammar>> = OnceLock::new();
    GRAMMARS.get_or_init(load_grammars).get(key)
}

fn load_grammars() -> HashMap<&'static str, Grammar> {
    #[allow(unused_mut)]
    let mut list: Vec<(&'static str, Language, String)> = Vec::new();
    #[cfg(feature = "lang-rust")]
    list.push((
        "rust",
        tree_sitter_rust::LANGUAGE.into(),
        tree_sitter_rust::TAGS_QUERY.into(),
    ));
    #[cfg(feature = "lang-javascript")]
    list.push((
        "javascript",
        tree_sitter_javascript::LANGUAGE.into(),
        tree_sitter_javascript::TAGS_QUERY.into(),
    ));
    #[cfg(feature = "lang-typescript")]
    {
        // TypeScript's tags build on JavaScript's.
        #[cfg(feature = "lang-javascript")]
        let base = format!(
            "{}\n{}",
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY
        );
        #[cfg(not(feature = "lang-javascript"))]
        let base = tree_sitter_typescript::TAGS_QUERY.to_owned();
        list.push((
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            base.clone(),
        ));
        list.push(("tsx", tree_sitter_typescript::LANGUAGE_TSX.into(), base));
    }
    #[cfg(feature = "lang-python")]
    list.push((
        "python",
        tree_sitter_python::LANGUAGE.into(),
        tree_sitter_python::TAGS_QUERY.into(),
    ));
    #[cfg(feature = "lang-go")]
    list.push((
        "go",
        tree_sitter_go::LANGUAGE.into(),
        tree_sitter_go::TAGS_QUERY.into(),
    ));
    #[cfg(feature = "lang-c")]
    list.push((
        "c",
        tree_sitter_c::LANGUAGE.into(),
        tree_sitter_c::TAGS_QUERY.into(),
    ));
    #[cfg(feature = "lang-cpp")]
    list.push((
        "cpp",
        tree_sitter_cpp::LANGUAGE.into(),
        tree_sitter_cpp::TAGS_QUERY.into(),
    ));
    #[cfg(feature = "lang-java")]
    list.push((
        "java",
        tree_sitter_java::LANGUAGE.into(),
        tree_sitter_java::TAGS_QUERY.into(),
    ));

    let mut grammars = HashMap::new();
    for (key, language, source) in list {
        // Only the definition patterns matter here; a query that doesn't compile for
        // this grammar (mixed JS/TS patterns) falls back to the grammar's own.
        match Query::new(&language, &source) {
            Ok(query) => {
                grammars.insert(key, Grammar { language, query });
            }
            Err(error) => eprintln!("crack: tags query for {key} didn't compile: {error}"),
        }
    }
    grammars
}

/// The definitions in one file.
pub fn extract(path: &str, source: &str) -> Vec<Symbol> {
    let Some(grammar) = grammar(Path::new(path)) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&grammar.language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let names = grammar.query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&grammar.query, tree.root_node(), source.as_bytes());
    let mut symbols: Vec<Symbol> = Vec::new();
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut definition = None;
        for capture in m.captures() {
            let capture_name = names[capture.index as usize];
            if capture_name == "name" {
                name = capture.node.utf8_text(source.as_bytes()).ok();
            } else if let Some(kind) = capture_name.strip_prefix("definition.") {
                definition = Some((kind, capture.node));
            }
        }
        if let (Some(name), Some((kind, node))) = (name, definition) {
            let symbol = Symbol {
                path: path.to_owned(),
                name: name.to_owned(),
                kind: kind.to_owned(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
            };
            // One entry per definition: a method also matches the plain function pattern.
            match symbols.iter_mut().find(|s| {
                s.name == symbol.name
                    && s.start_line == symbol.start_line
                    && s.end_line == symbol.end_line
            }) {
                Some(existing) => {
                    if existing.kind == "function" && symbol.kind != "function" {
                        existing.kind = symbol.kind;
                    }
                }
                None => symbols.push(symbol),
            }
        }
    }
    symbols.sort_by_key(|s| (s.start_line, std::cmp::Reverse(s.end_line)));
    symbols
}

/// The project index.
pub struct Index {
    root: PathBuf,
    db: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS code_files (
    project TEXT NOT NULL, path TEXT NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL,
    PRIMARY KEY (project, path));
CREATE TABLE IF NOT EXISTS code_symbols (
    project TEXT NOT NULL, path TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
    start_line INTEGER NOT NULL, end_line INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS code_symbols_name ON code_symbols (project, name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS code_symbols_path ON code_symbols (project, path);
";

/// Files bigger than this aren't indexed (generated code, bundles).
const MAX_FILE: u64 = 1_000_000;

impl Index {
    pub fn open(root: &Path) -> anyhow::Result<Index> {
        Self::open_at(root, &crate::config::data_dir().join("crack.db"))
    }

    pub fn open_at(root: &Path, db: &Path) -> anyhow::Result<Index> {
        if let Some(dir) = db.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let connection = Connection::open(db)?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(SCHEMA)?;
        Ok(Index {
            root: root.to_owned(),
            db: Mutex::new(connection),
        })
    }

    fn project(&self) -> String {
        self.root.display().to_string()
    }

    /// Brings the index up to date with the files on disk. Returns how many files
    /// were (re)parsed.
    pub fn refresh(&self) -> anyhow::Result<usize> {
        let project = self.project();
        let mut db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut known: HashMap<String, (i64, i64)> = HashMap::new();
        {
            let mut statement =
                db.prepare("SELECT path, mtime, size FROM code_files WHERE project = ?1")?;
            let rows = statement.query_map(params![project], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
            })?;
            for row in rows.flatten() {
                known.insert(row.0, row.1);
            }
        }
        let mut changed = Vec::new();
        let walker = ignore::WalkBuilder::new(&self.root).hidden(true).build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) || grammar(path).is_none() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_FILE {
                continue;
            }
            let relative = path
                .strip_prefix(&self.root)
                .unwrap_or(path)
                .display()
                .to_string();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i64);
            let stamp = (mtime, meta.len() as i64);
            if known.remove(&relative) != Some(stamp) {
                changed.push((relative, path.to_owned(), stamp));
            }
        }
        let gone: Vec<String> = known.into_keys().collect();
        if changed.is_empty() && gone.is_empty() {
            return Ok(0);
        }
        // Parse outside the transaction; it's the slow part.
        let parsed: Vec<(String, (i64, i64), Vec<Symbol>)> = changed
            .into_iter()
            .map(|(relative, path, stamp)| {
                let source = std::fs::read_to_string(&path).unwrap_or_default();
                let symbols = extract(&relative, &source);
                (relative, stamp, symbols)
            })
            .collect();
        let tx = db.transaction()?;
        for path in &gone {
            tx.execute(
                "DELETE FROM code_files WHERE project = ?1 AND path = ?2",
                params![project, path],
            )?;
            tx.execute(
                "DELETE FROM code_symbols WHERE project = ?1 AND path = ?2",
                params![project, path],
            )?;
        }
        for (path, (mtime, size), symbols) in &parsed {
            tx.execute(
                "DELETE FROM code_symbols WHERE project = ?1 AND path = ?2",
                params![project, path],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO code_files (project, path, mtime, size) VALUES (?1, ?2, ?3, ?4)",
                params![project, path, mtime, size],
            )?;
            for s in symbols {
                tx.execute(
                    "INSERT INTO code_symbols (project, path, name, kind, start_line, end_line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![project, path, s.name, s.kind, s.start_line as i64, s.end_line as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(parsed.len() + gone.len())
    }

    fn query(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> anyhow::Result<Vec<Symbol>> {
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut statement = db.prepare(sql)?;
        let rows = statement.query_map(args, |r| {
            Ok(Symbol {
                path: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                start_line: r.get::<_, i64>(3)? as usize,
                end_line: r.get::<_, i64>(4)? as usize,
            })
        })?;
        Ok(rows.flatten().collect())
    }

    pub fn in_file(&self, path: &str) -> anyhow::Result<Vec<Symbol>> {
        self.query(
            "SELECT path, name, kind, start_line, end_line FROM code_symbols
             WHERE project = ?1 AND path = ?2 ORDER BY start_line, end_line DESC",
            &[&self.project(), &path],
        )
    }

    /// Exact (case-insensitive) matches first, then names containing `name`.
    pub fn find(
        &self,
        name: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Symbol>> {
        let pattern = format!("%{}%", name.replace('%', "").replace('_', "\\_"));
        let mut found = self.query(
            "SELECT path, name, kind, start_line, end_line FROM code_symbols
             WHERE project = ?1 AND name LIKE ?2 ESCAPE '\\'
             ORDER BY (name = ?3 COLLATE NOCASE) DESC, length(name), path, start_line LIMIT ?4",
            &[&self.project(), &pattern, &name, &((limit * 4) as i64)],
        )?;
        if let Some(kind) = kind {
            found.retain(|s| s.kind.eq_ignore_ascii_case(kind));
        }
        found.truncate(limit);
        Ok(found)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// An outline: definitions nested by where they sit in the file.
pub fn outline(symbols: &[Symbol]) -> String {
    let mut out = String::new();
    let mut stack: Vec<usize> = Vec::new();
    for s in symbols {
        while stack.last().is_some_and(|&end| s.start_line > end) {
            stack.pop();
        }
        out.push_str(&format!(
            "{}{} {} (lines {}-{})\n",
            "  ".repeat(stack.len()),
            s.kind,
            s.name,
            s.start_line,
            s.end_line
        ));
        if s.end_line > s.start_line {
            stack.push(s.end_line);
        }
    }
    out
}

/// The index for a project, opened once per process.
pub fn for_project(cwd: &Path) -> anyhow::Result<std::sync::Arc<Index>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, std::sync::Arc<Index>>>> = OnceLock::new();
    let root = crate::context::project_root(cwd);
    let mut open = OPEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(index) = open.get(&root) {
        return Ok(index.clone());
    }
    let index = std::sync::Arc::new(Index::open(&root)?);
    open.insert(root, index.clone());
    Ok(index)
}

/// The code tools' definitions.
pub fn specs() -> Vec<crate::providers::ToolSpec> {
    use serde_json::json;
    let spec =
        |name: &str, description: &str, schema: serde_json::Value| crate::providers::ToolSpec {
            name: name.into(),
            description: description.into(),
            schema,
        };
    vec![
        spec(
            "symbols",
            "Outline a source file: its functions, types, methods and modules with line ranges, without reading the whole file.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        ),
        spec(
            "find_symbol",
            "Find where functions, types, methods or modules are defined across the project, by name (exact matches first, \
             then names containing it). Faster and more precise than grep for definitions.",
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "kind": {"type": "string", "description": "Only this kind: function, method, class, interface, module, macro, type, constant."}
            }, "required": ["name"]}),
        ),
        spec(
            "read_symbol",
            "Read just one definition's source (a function, type or method), with line numbers, instead of the whole file.",
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "path": {"type": "string", "description": "The file, when the name is defined in more than one place."}
            }, "required": ["name"]}),
        ),
    ]
}

/// Runs one of the code tools.
pub fn tool(name: &str, input: &serde_json::Value, cwd: &Path) -> anyhow::Result<(String, String)> {
    let index = for_project(cwd)?;
    index.refresh()?;
    let root = index.root().to_owned();
    let relative = |path: &str| -> String {
        let full = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            cwd.join(path)
        };
        full.strip_prefix(&root)
            .unwrap_or(&full)
            .display()
            .to_string()
    };
    let arg = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match name {
        "symbols" => {
            let path = relative(arg("path").ok_or_else(|| anyhow::anyhow!("missing path"))?);
            if grammar(Path::new(&path)).is_none() {
                anyhow::bail!(
                    "no symbol support for {path} (supported: Rust, TypeScript, JavaScript, Python, Go, C, C++, Java)"
                );
            }
            let symbols = index.in_file(&path)?;
            let text = if symbols.is_empty() {
                format!("No definitions found in {path}.")
            } else {
                outline(&symbols)
            };
            Ok((text, format!("Symbols in {path} ({})", symbols.len())))
        }
        "find_symbol" => {
            let wanted = arg("name").ok_or_else(|| anyhow::anyhow!("missing name"))?;
            let found = index.find(wanted, arg("kind"), 100)?;
            let text = if found.is_empty() {
                format!("No definitions named like '{wanted}'.")
            } else {
                found
                    .iter()
                    .map(|s| format!("{}:{} {} {}", s.path, s.start_line, s.kind, s.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((
                text,
                format!("Find symbol \"{wanted}\" ({} found)", found.len()),
            ))
        }
        "read_symbol" => {
            let wanted = arg("name").ok_or_else(|| anyhow::anyhow!("missing name"))?;
            let mut found: Vec<Symbol> = index
                .find(wanted, None, 200)?
                .into_iter()
                .filter(|s| s.name == wanted)
                .collect();
            if let Some(path) = arg("path") {
                let path = relative(path);
                found.retain(|s| s.path == path);
            }
            match found.len() {
                0 => anyhow::bail!("no definition named '{wanted}'; try find_symbol"),
                1 => {}
                _ => {
                    let places: Vec<String> = found
                        .iter()
                        .map(|s| format!("{}:{} ({})", s.path, s.start_line, s.kind))
                        .collect();
                    return Ok((
                        format!(
                            "'{wanted}' is defined in several places; pass `path`:\n{}",
                            places.join("\n")
                        ),
                        format!("Read symbol {wanted} ({} matches)", found.len()),
                    ));
                }
            }
            let s = &found[0];
            let text = std::fs::read_to_string(root.join(&s.path))?;
            let body: Vec<String> = text
                .lines()
                .enumerate()
                .skip(s.start_line - 1)
                .take(s.end_line + 1 - s.start_line)
                .map(|(i, line)| format!("{:>6}\t{line}", i + 1))
                .collect();
            Ok((
                format!(
                    "{}:{}-{}\n{}",
                    s.path,
                    s.start_line,
                    s.end_line,
                    body.join("\n")
                ),
                format!("Read {} {} ({}:{})", s.kind, s.name, s.path, s.start_line),
            ))
        }
        other => anyhow::bail!("unknown code tool {other}"),
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_definitions_and_incremental_index() {
        let source = "struct Point { x: i32 }\n\nimpl Point {\n    fn norm(&self) -> i32 {\n        self.x\n    }\n}\n\nfn main() {}\n";
        let symbols = extract("src/main.rs", source);
        let names: Vec<(&str, &str)> = symbols
            .iter()
            .map(|s| (s.kind.as_str(), s.name.as_str()))
            .collect();
        assert!(names.contains(&("class", "Point")), "{names:?}");
        assert!(names.contains(&("method", "norm")), "{names:?}");
        assert!(names.contains(&("function", "main")), "{names:?}");
        assert!(
            outline(&symbols).contains("method norm (lines 4-6)"),
            "{}",
            outline(&symbols)
        );
        assert_eq!(symbols.iter().filter(|s| s.name == "norm").count(), 1);

        let root = std::env::temp_dir().join(format!("crack-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), source).unwrap();
        let index = Index::open_at(&root, &root.join("test.db")).unwrap();
        assert_eq!(index.refresh().unwrap(), 1);
        assert_eq!(index.refresh().unwrap(), 0);
        assert_eq!(index.find("norm", None, 10).unwrap()[0].start_line, 4);
        // An edit is picked up on the next refresh; a deleted file disappears.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("src/main.rs"), "fn renamed() {}\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
        assert_eq!(index.refresh().unwrap(), 2);
        assert!(index.find("norm", None, 10).unwrap().is_empty());
        assert_eq!(index.find("renamed", None, 10).unwrap().len(), 1);
        std::fs::remove_file(root.join("src/lib.rs")).unwrap();
        index.refresh().unwrap();
        assert!(index.find("helper", None, 10).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(feature = "lang-typescript", feature = "lang-python"))]
    #[test]
    fn typescript_and_python() {
        let ts = extract(
            "a.ts",
            "export class Cart { total(): number { return 0 } }\ninterface Item { id: string }\nfunction add(a: number) { return a }\n",
        );
        let names: Vec<&str> = ts.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Cart") && names.contains(&"add") && names.contains(&"Item"),
            "{names:?}"
        );
        let py = extract(
            "a.py",
            "class Cart:\n    def total(self):\n        return 0\n\ndef add(a):\n    return a\n",
        );
        let names: Vec<&str> = py.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Cart", "total", "add"]);
    }
}
