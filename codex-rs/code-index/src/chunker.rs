use anyhow::Result;
use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::approx_token_count;
use codex_utils_string::take_bytes_at_char_boundary;
use sha2::Digest;
use sha2::Sha256;
use tree_sitter::Node;
use tree_sitter::Parser;

use crate::language::CodeLanguage;

pub(crate) const DEFAULT_CHUNK_MAX_TOKENS: usize = 800;
pub(crate) const DEFAULT_CHUNK_OVERLAP_LINES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeChunk {
    pub symbol: Option<String>,
    pub parent_symbol: Option<String>,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub content_hash: String,
}

pub(crate) fn chunk_file(
    language: CodeLanguage,
    source: &str,
    max_tokens: usize,
    overlap_lines: usize,
) -> Result<Vec<CodeChunk>> {
    let Some(grammar) = language.grammar() else {
        return Ok(fallback_chunks(source, max_tokens, overlap_lines));
    };

    let mut parser = Parser::new();
    parser.set_language(&grammar)?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(fallback_chunks(source, max_tokens, overlap_lines));
    };

    let mut chunks = Vec::new();
    walk_node(
        tree.root_node(),
        source.as_bytes(),
        language,
        max_tokens,
        overlap_lines,
        &mut chunks,
    );
    if chunks.is_empty() {
        Ok(fallback_chunks(source, max_tokens, overlap_lines))
    } else {
        Ok(chunks)
    }
}

fn walk_node(
    node: Node<'_>,
    source: &[u8],
    language: CodeLanguage,
    max_tokens: usize,
    overlap_lines: usize,
    chunks: &mut Vec<CodeChunk>,
) {
    let kind = node.kind();
    let is_container_with_children =
        language.is_container_node(kind) && contains_chunk_descendant(node, language);
    if language.is_chunk_node(kind) && !is_container_with_children {
        push_node_chunks(node, source, language, max_tokens, overlap_lines, chunks);
        return;
    }

    for index in 0..node.named_child_count() {
        if let Some(child) = node.named_child(index) {
            walk_node(child, source, language, max_tokens, overlap_lines, chunks);
        }
    }
}

fn contains_chunk_descendant(node: Node<'_>, language: CodeLanguage) -> bool {
    for index in 0..node.named_child_count() {
        let Some(child) = node.named_child(index) else {
            continue;
        };
        if should_chunk_node(child, language) || contains_chunk_descendant(child, language) {
            return true;
        }
    }
    false
}

fn should_chunk_node(node: Node<'_>, language: CodeLanguage) -> bool {
    if !language.is_chunk_node(node.kind()) {
        return false;
    }
    if !matches!(
        language,
        CodeLanguage::TypeScript | CodeLanguage::Tsx | CodeLanguage::JavaScript | CodeLanguage::Jsx
    ) || node.kind() != "variable_declarator"
    {
        return true;
    }

    node.child_by_field_name("value").is_some_and(|value| {
        matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "function"
        )
    })
}

fn push_node_chunks(
    node: Node<'_>,
    source: &[u8],
    language: CodeLanguage,
    max_tokens: usize,
    overlap_lines: usize,
    chunks: &mut Vec<CodeChunk>,
) {
    if !should_chunk_node(node, language) {
        return;
    }
    let Ok(text) = node.utf8_text(source) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }

    let symbol = symbol_for_node(node, source);
    let parent_symbol = parent_symbol_for_node(node, source, language);
    let start_line = node.start_position().row + 1;
    for part in split_text(text, start_line, max_tokens, overlap_lines) {
        chunks.push(CodeChunk {
            symbol: symbol.clone(),
            parent_symbol: parent_symbol.clone(),
            kind: node.kind().to_string(),
            start_line: part.start_line,
            end_line: part.end_line,
            content_hash: hash_text(&part.content),
            content: part.content,
        });
    }
}

fn symbol_for_node(node: Node<'_>, source: &[u8]) -> Option<String> {
    for field in ["name", "type", "declarator"] {
        let Some(candidate) = node.child_by_field_name(field) else {
            continue;
        };
        if let Some(symbol) = symbol_from_node(candidate, source) {
            return Some(symbol);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(symbol) = symbol_from_node(child, source) {
            return Some(symbol);
        }
    }
    None
}

fn symbol_from_node(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "property_identifier"
        | "type_identifier"
        | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern" => node
            .utf8_text(source)
            .ok()
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .map(ToString::to_string),
        _ => {
            for field in ["name", "type", "declarator"] {
                if let Some(candidate) = node.child_by_field_name(field)
                    && let Some(symbol) = symbol_from_node(candidate, source)
                {
                    return Some(symbol);
                }
            }
            None
        }
    }
}

fn parent_symbol_for_node(node: Node<'_>, source: &[u8], language: CodeLanguage) -> Option<String> {
    let node_symbol = symbol_for_node(node, source);
    let mut current = node.parent();
    while let Some(parent) = current {
        if (language.is_chunk_node(parent.kind()) || language.is_container_node(parent.kind()))
            && let Some(symbol) = symbol_for_node(parent, source)
            && Some(symbol.as_str()) != node_symbol.as_deref()
        {
            return Some(symbol);
        }
        current = parent.parent();
    }
    None
}

fn fallback_chunks(source: &str, max_tokens: usize, overlap_lines: usize) -> Vec<CodeChunk> {
    split_text(source, 1, max_tokens, overlap_lines)
        .into_iter()
        .map(|part| CodeChunk {
            symbol: None,
            parent_symbol: None,
            kind: "text".to_string(),
            start_line: part.start_line,
            end_line: part.end_line,
            content_hash: hash_text(&part.content),
            content: part.content,
        })
        .collect()
}

struct TextPart {
    start_line: usize,
    end_line: usize,
    content: String,
}

fn split_text(
    text: &str,
    start_line: usize,
    max_tokens: usize,
    overlap_lines: usize,
) -> Vec<TextPart> {
    let max_tokens = max_tokens.max(1);
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }

    let max_bytes = approx_bytes_for_tokens(max_tokens).max(1);
    let mut parts = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let mut end = start;
        let mut content = String::new();
        while end < lines.len() {
            let line = lines[end];
            if end > start && approx_token_count(&format!("{content}{line}")) > max_tokens {
                break;
            }
            content.push_str(line);
            end += 1;
            if end == start + 1 && approx_token_count(&content) > max_tokens {
                break;
            }
        }

        if end == start {
            end += 1;
        }

        if end == start + 1 && approx_token_count(&content) > max_tokens {
            for line_part in split_long_line(&content, max_bytes) {
                let line_number = start_line + start;
                parts.push(TextPart {
                    start_line: line_number,
                    end_line: line_number,
                    content: line_part,
                });
            }
        } else {
            parts.push(TextPart {
                start_line: start_line + start,
                end_line: start_line + end.saturating_sub(1),
                content,
            });
        }

        if end == lines.len() {
            break;
        }
        let next_start = end.saturating_sub(overlap_lines);
        start = if next_start <= start { end } else { next_start };
    }
    parts
}

fn split_long_line(line: &str, max_bytes: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut remaining = line;
    while !remaining.is_empty() {
        let mut chunk = take_bytes_at_char_boundary(remaining, max_bytes);
        if chunk.is_empty() {
            let Some(first) = remaining.chars().next() else {
                parts.push(remaining.to_string());
                break;
            };
            chunk = &remaining[..first.len_utf8()];
        }
        parts.push(chunk.to_string());
        remaining = &remaining[chunk.len()..];
    }
    parts
}

pub(crate) fn hash_text(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn rust_chunking_preserves_function_symbols() {
        let source = r#"
struct Greeter {
    name: String,
}

impl Greeter {
    fn greet(&self) -> String {
        format!("hello {}", self.name)
    }
}
"#;
        let chunks = chunk_file(CodeLanguage::Rust, source, 200, 0).expect("chunk rust");
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.symbol.as_deref() == Some("greet")
                    && chunk.parent_symbol.as_deref() == Some("Greeter"))
        );
    }

    #[test]
    fn typescript_chunking_finds_methods() {
        let source = r#"
class Greeter {
    greet(name: string) {
        return `hello ${name}`;
    }
}
"#;
        let chunks = chunk_file(CodeLanguage::TypeScript, source, 200, 0).expect("chunk ts");
        assert!(chunks.iter().any(|chunk| {
            chunk.symbol.as_deref() == Some("greet")
                && chunk.parent_symbol.as_deref() == Some("Greeter")
        }));
    }

    #[test]
    fn python_chunking_finds_functions_and_methods() {
        let source = r#"
class Greeter:
    def greet(self, name):
        return f"hello {name}"

def standalone():
    return "hello"
"#;
        let chunks = chunk_file(CodeLanguage::Python, source, 200, 0).expect("chunk python");
        assert!(chunks.iter().any(|chunk| {
            chunk.symbol.as_deref() == Some("greet")
                && chunk.parent_symbol.as_deref() == Some("Greeter")
        }));
        assert!(chunks.iter().any(|chunk| {
            chunk.symbol.as_deref() == Some("standalone") && chunk.parent_symbol.is_none()
        }));
    }

    #[test]
    fn unknown_text_uses_line_fallback() {
        let source = "alpha\nbeta\ngamma\n";
        let chunks = chunk_file(CodeLanguage::Text, source, 2, 0).expect("chunk text");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
    }
}
