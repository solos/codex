use std::path::Path;

use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodeLanguage {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Text,
}

impl CodeLanguage {
    pub(crate) fn from_path(path: &Path) -> Self {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" => Self::JavaScript,
            "jsx" => Self::Jsx,
            "py" | "pyi" => Self::Python,
            _ => Self::Text,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::Python => "python",
            Self::Text => "text",
        }
    }

    pub(crate) fn grammar(self) -> Option<Language> {
        match self {
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            // The TSX grammar is a superset of ordinary JavaScript and handles JSX.
            Self::Tsx | Self::JavaScript | Self::Jsx => {
                Some(tree_sitter_typescript::LANGUAGE_TSX.into())
            }
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Self::Text => None,
        }
    }

    pub(crate) fn is_chunk_node(self, kind: &str) -> bool {
        match self {
            Self::Rust => matches!(
                kind,
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "impl_item"
                    | "mod_item"
                    | "type_item"
                    | "const_item"
                    | "static_item"
                    | "macro_definition"
            ),
            Self::TypeScript | Self::Tsx | Self::JavaScript | Self::Jsx => matches!(
                kind,
                "function_declaration"
                    | "class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "method_definition"
                    | "variable_declarator"
            ),
            Self::Python => matches!(kind, "function_definition" | "class_definition"),
            Self::Text => false,
        }
    }

    pub(crate) fn is_container_node(self, kind: &str) -> bool {
        match self {
            Self::Rust => matches!(kind, "impl_item" | "trait_item" | "mod_item"),
            Self::TypeScript | Self::Tsx | Self::JavaScript | Self::Jsx => {
                matches!(kind, "class_declaration")
            }
            Self::Python => matches!(kind, "class_definition"),
            Self::Text => false,
        }
    }
}
