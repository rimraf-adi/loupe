use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Class,
    Interface,
    Trait,
    Enum,
    Impl,
    Module,
    Constant,
    Variable,
    Type,
    Macro,
}

impl SymbolKind {
    pub const ALL: [SymbolKind; 13] = [
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Struct,
        SymbolKind::Class,
        SymbolKind::Interface,
        SymbolKind::Trait,
        SymbolKind::Enum,
        SymbolKind::Impl,
        SymbolKind::Module,
        SymbolKind::Constant,
        SymbolKind::Variable,
        SymbolKind::Type,
        SymbolKind::Macro,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Struct => "struct",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Enum => "enum",
            SymbolKind::Impl => "impl",
            SymbolKind::Module => "module",
            SymbolKind::Constant => "constant",
            SymbolKind::Variable => "variable",
            SymbolKind::Type => "type",
            SymbolKind::Macro => "macro",
        }
    }

    pub fn parse(s: &str) -> Option<SymbolKind> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub(crate) fn to_u8(self) -> u8 {
        Self::ALL.iter().position(|&k| k == self).unwrap() as u8
    }

    pub(crate) fn from_u8(v: u8) -> Option<SymbolKind> {
        Self::ALL.get(v as usize).copied()
    }

    /// Kinds whose bodies hold member functions.
    pub(crate) fn is_container(self) -> bool {
        matches!(
            self,
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Impl
                | SymbolKind::Trait
                | SymbolKind::Interface
                | SymbolKind::Enum
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Private,
    Protected,
    /// Java-style package-private.
    Package,
    /// The language has no convention we know, or the symbol came from the generic fallback.
    Unknown,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Protected => "protected",
            Visibility::Package => "package",
            Visibility::Unknown => "unknown",
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Visibility::Public => 0,
            Visibility::Private => 1,
            Visibility::Protected => 2,
            Visibility::Package => 3,
            Visibility::Unknown => 4,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Option<Visibility> {
        Some(match v {
            0 => Visibility::Public,
            1 => Visibility::Private,
            2 => Visibility::Protected,
            3 => Visibility::Package,
            4 => Visibility::Unknown,
            _ => return None,
        })
    }
}

/// One definition found in a file. Line numbers are 1-based and inclusive; byte offsets are into
/// the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    /// Index (within the same file's symbol list) of the enclosing definition.
    pub parent: Option<u32>,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    /// The declaration without its body, whitespace-collapsed.
    pub signature: String,
    pub doc: Option<String>,
}
