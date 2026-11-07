use std::path::Path;
use tree_sitter::{Language, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Python,
    TypeScript,
    Tsx,
}

impl SupportedLanguage {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        match ext {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" | "jsx" | "js" | "mjs" | "cjs" => Some(Self::Tsx),
            _ => None,
        }
    }

    pub fn tree_sitter_language(&self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    pub fn make_parser(&self) -> Option<Parser> {
        let mut parser = Parser::new();
        let lang = self.tree_sitter_language();
        parser.set_language(&lang).ok()?;
        Some(parser)
    }
}
