//! Syntax highlighting for code blocks, from each tree-sitter grammar's own
//! `highlights.scm` (the same grammars as the code index, one Cargo feature each).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

#[allow(unused_imports)]
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

/// What a span of code is, reduced from the grammars' many capture names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Constant,
}

fn kind(capture: &str) -> Option<Kind> {
    Some(match capture.split('.').next()? {
        "keyword" | "conditional" | "repeat" | "include" | "storageclass" | "exception" => {
            Kind::Keyword
        }
        "string" | "character" | "escape" => Kind::String,
        "comment" => Kind::Comment,
        "function" | "method" | "constructor" => Kind::Function,
        "type" | "attribute" | "tag" => Kind::Type,
        "constant" | "number" | "float" | "boolean" => Kind::Constant,
        _ => return None,
    })
}

struct Highlighter {
    language: Language,
    query: Query,
    kinds: Vec<Option<Kind>>,
}

/// The grammar for a fence's info string (` ```rust `, ` ```py `, ...).
fn grammar_key(info: &str) -> Option<&'static str> {
    let lang = info
        .split([' ', ',', '{'])
        .next()?
        .trim()
        .to_ascii_lowercase();
    match lang.as_str() {
        #[cfg(feature = "lang-rust")]
        "rust" | "rs" => Some("rust"),
        #[cfg(feature = "lang-typescript")]
        "ts" | "typescript" | "mts" | "cts" => Some("typescript"),
        #[cfg(feature = "lang-typescript")]
        "tsx" => Some("tsx"),
        #[cfg(feature = "lang-javascript")]
        "js" | "javascript" | "jsx" | "mjs" | "cjs" | "node" => Some("javascript"),
        #[cfg(feature = "lang-python")]
        "py" | "python" | "python3" | "pyi" => Some("python"),
        #[cfg(feature = "lang-go")]
        "go" | "golang" => Some("go"),
        #[cfg(feature = "lang-c")]
        "c" | "h" => Some("c"),
        #[cfg(feature = "lang-cpp")]
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" => Some("cpp"),
        #[cfg(feature = "lang-java")]
        "java" => Some("java"),
        _ => None,
    }
}

fn load() -> HashMap<&'static str, Highlighter> {
    #[allow(unused_mut)]
    let mut list: Vec<(&'static str, Language, String)> = Vec::new();
    #[cfg(feature = "lang-rust")]
    list.push((
        "rust",
        tree_sitter_rust::LANGUAGE.into(),
        tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
    ));
    #[cfg(feature = "lang-javascript")]
    list.push((
        "javascript",
        tree_sitter_javascript::LANGUAGE.into(),
        format!(
            "{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
        ),
    ));
    #[cfg(feature = "lang-typescript")]
    {
        // TypeScript's highlights only add to JavaScript's; its own come first so
        // they take priority.
        #[cfg(feature = "lang-javascript")]
        let base = format!(
            "{}\n{}",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            tree_sitter_javascript::HIGHLIGHT_QUERY
        );
        #[cfg(not(feature = "lang-javascript"))]
        let base = tree_sitter_typescript::HIGHLIGHTS_QUERY.to_owned();
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
        tree_sitter_python::HIGHLIGHTS_QUERY.into(),
    ));
    #[cfg(feature = "lang-go")]
    list.push((
        "go",
        tree_sitter_go::LANGUAGE.into(),
        tree_sitter_go::HIGHLIGHTS_QUERY.into(),
    ));
    #[cfg(feature = "lang-c")]
    list.push((
        "c",
        tree_sitter_c::LANGUAGE.into(),
        tree_sitter_c::HIGHLIGHT_QUERY.into(),
    ));
    #[cfg(feature = "lang-cpp")]
    {
        // C++'s highlights add to C's.
        #[cfg(feature = "lang-c")]
        let source = format!(
            "{}\n{}",
            tree_sitter_cpp::HIGHLIGHT_QUERY,
            tree_sitter_c::HIGHLIGHT_QUERY
        );
        #[cfg(not(feature = "lang-c"))]
        let source = tree_sitter_cpp::HIGHLIGHT_QUERY.to_owned();
        list.push(("cpp", tree_sitter_cpp::LANGUAGE.into(), source));
    }
    #[cfg(feature = "lang-java")]
    list.push((
        "java",
        tree_sitter_java::LANGUAGE.into(),
        tree_sitter_java::HIGHLIGHTS_QUERY.into(),
    ));

    let mut map = HashMap::new();
    for (key, language, source) in list {
        match Query::new(&language, &source) {
            Ok(query) => {
                let kinds = query.capture_names().iter().map(|n| kind(n)).collect();
                map.insert(
                    key,
                    Highlighter {
                        language,
                        query,
                        kinds,
                    },
                );
            }
            Err(error) => tracing::warn!(key, %error, "highlight query didn't compile"),
        }
    }
    map
}

/// Highlighted spans of `code`, in order and not overlapping. Empty for an unknown
/// language.
pub fn highlight(info: &str, code: &str) -> Vec<(Range<usize>, Kind)> {
    static HIGHLIGHTERS: OnceLock<HashMap<&'static str, Highlighter>> = OnceLock::new();
    let Some(h) = grammar_key(info).and_then(|key| HIGHLIGHTERS.get_or_init(load).get(key)) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&h.language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(code, None) else {
        return Vec::new();
    };
    // Every capture, then painted widest first and, for the same node, last pattern
    // first: so inner nodes win, and earlier patterns (higher priority) win ties.
    let mut found: Vec<(Range<usize>, usize, Kind)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(&h.query, tree.root_node(), code.as_bytes());
    while let Some((m, index)) = captures.next() {
        let capture = m.captures()[*index];
        if let Some(kind) = h.kinds[capture.index as usize] {
            found.push((capture.node.byte_range(), m.pattern_index, kind));
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse((f.0.len(), f.1)));
    let mut paint: Vec<Option<Kind>> = vec![None; code.len()];
    for (range, _, kind) in found {
        for slot in &mut paint[range] {
            *slot = Some(kind);
        }
    }
    let mut spans: Vec<(Range<usize>, Kind)> = Vec::new();
    let mut i = 0;
    while i < paint.len() {
        let Some(kind) = paint[i] else {
            i += 1;
            continue;
        };
        let start = i;
        while i < paint.len() && paint[i] == Some(kind) {
            i += 1;
        }
        spans.push((start..i, kind));
    }
    // Spans must sit on character boundaries for the text renderer.
    spans.retain(|(r, _)| code.is_char_boundary(r.start) && code.is_char_boundary(r.end));
    spans
}

#[cfg(all(test, feature = "lang-rust"))]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_strings_comments() {
        let code = "// hi\nfn main() { let s = \"x\"; }";
        let spans = highlight("rust", code);
        let at = |text: &str| {
            let start = code.find(text).unwrap();
            spans
                .iter()
                .find(|(r, _)| r.start <= start && start < r.end)
                .map(|(_, k)| *k)
        };
        assert_eq!(at("// hi"), Some(Kind::Comment));
        assert_eq!(at("fn"), Some(Kind::Keyword));
        assert_eq!(at("main"), Some(Kind::Function));
        assert_eq!(at("\"x\""), Some(Kind::String));
        assert!(highlight("klingon", code).is_empty());
    }

    #[cfg(feature = "languages")]
    #[test]
    fn every_grammar_query_compiles() {
        let loaded = load();
        let mut keys: Vec<_> = loaded.keys().copied().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["c", "cpp", "go", "java", "javascript", "python", "rust", "tsx", "typescript"]
        );
        for (fence, code) in [
            ("py", "def f():\n    return \"x\""),
            ("ts", "const x: number = 1; function f() {}"),
            ("go", "package main\nfunc main() {}"),
            ("cpp", "int main() { return 0; }"),
            ("java", "class A { void f() {} }"),
        ] {
            assert!(!highlight(fence, code).is_empty(), "{fence}");
        }
    }
}
