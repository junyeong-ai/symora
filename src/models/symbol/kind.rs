use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Closely mirrors LSP `SymbolKind` so the model layer can pass through
/// LSP responses with no lossy translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

impl SymbolKind {
    pub fn from_lsp(kind: u32) -> Self {
        match kind {
            1 => Self::File,
            2 => Self::Module,
            3 => Self::Namespace,
            4 => Self::Package,
            5 => Self::Class,
            6 => Self::Method,
            7 => Self::Property,
            8 => Self::Field,
            9 => Self::Constructor,
            10 => Self::Enum,
            11 => Self::Interface,
            12 => Self::Function,
            13 => Self::Variable,
            14 => Self::Constant,
            15 => Self::String,
            16 => Self::Number,
            17 => Self::Boolean,
            18 => Self::Array,
            19 => Self::Object,
            20 => Self::Key,
            21 => Self::Null,
            22 => Self::EnumMember,
            23 => Self::Struct,
            24 => Self::Event,
            25 => Self::Operator,
            26 => Self::TypeParameter,
            _ => Self::Variable,
        }
    }

    pub fn is_callable(&self) -> bool {
        matches!(self, Self::Function | Self::Method | Self::Constructor)
    }

    pub fn is_low_level(&self) -> bool {
        matches!(
            self,
            Self::Variable
                | Self::Constant
                | Self::String
                | Self::Number
                | Self::Boolean
                | Self::Array
                | Self::Object
                | Self::Key
                | Self::Null
        )
    }

    pub fn parse_or_default(s: &str) -> Self {
        s.parse().unwrap_or(Self::Variable)
    }

    /// All valid kind names (used for error messages and CLI help).
    pub fn all_kind_names() -> &'static [&'static str] {
        &[
            "function",
            "class",
            "method",
            "field",
            "variable",
            "constant",
            "interface",
            "trait",
            "enum",
            "struct",
            "module",
            "property",
            "constructor",
            "enum_member",
            "type_parameter",
        ]
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::File => "file",
            Self::Module => "module",
            Self::Namespace => "namespace",
            Self::Package => "package",
            Self::Class => "class",
            Self::Method => "method",
            Self::Property => "property",
            Self::Field => "field",
            Self::Constructor => "constructor",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
            Self::Key => "key",
            Self::Null => "null",
            Self::EnumMember => "enum_member",
            Self::Struct => "struct",
            Self::Event => "event",
            Self::Operator => "operator",
            Self::TypeParameter => "type_parameter",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for SymbolKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "file" => Ok(Self::File),
            "module" => Ok(Self::Module),
            "namespace" => Ok(Self::Namespace),
            "package" => Ok(Self::Package),
            "class" => Ok(Self::Class),
            "method" => Ok(Self::Method),
            "property" => Ok(Self::Property),
            "field" => Ok(Self::Field),
            "constructor" => Ok(Self::Constructor),
            "enum" => Ok(Self::Enum),
            // `trait` is aliased to Interface (rust-analyzer reports traits as Interface).
            "interface" | "trait" => Ok(Self::Interface),
            "function" => Ok(Self::Function),
            "variable" => Ok(Self::Variable),
            "constant" => Ok(Self::Constant),
            "struct" => Ok(Self::Struct),
            "enum_member" | "enummember" => Ok(Self::EnumMember),
            "type_parameter" | "typeparameter" => Ok(Self::TypeParameter),
            _ => Err(format!("Unknown symbol kind: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_lsp_maps_known_codes() {
        assert_eq!(SymbolKind::from_lsp(5), SymbolKind::Class);
        assert_eq!(SymbolKind::from_lsp(12), SymbolKind::Function);
        assert_eq!(SymbolKind::from_lsp(6), SymbolKind::Method);
    }

    #[test]
    fn from_lsp_unknown_falls_back_to_variable() {
        assert_eq!(SymbolKind::from_lsp(999), SymbolKind::Variable);
    }

    #[test]
    fn is_low_level_classifies_value_types() {
        assert!(SymbolKind::Variable.is_low_level());
        assert!(SymbolKind::Constant.is_low_level());
        assert!(SymbolKind::String.is_low_level());
        assert!(SymbolKind::Number.is_low_level());
        assert!(!SymbolKind::Function.is_low_level());
        assert!(!SymbolKind::Class.is_low_level());
        assert!(!SymbolKind::Method.is_low_level());
    }

    #[test]
    fn is_callable_covers_function_method_constructor() {
        assert!(SymbolKind::Function.is_callable());
        assert!(SymbolKind::Method.is_callable());
        assert!(SymbolKind::Constructor.is_callable());
        assert!(!SymbolKind::Class.is_callable());
        assert!(!SymbolKind::Variable.is_callable());
    }

    #[test]
    fn from_str_aliases_trait_to_interface() {
        assert_eq!(
            "trait".parse::<SymbolKind>().unwrap(),
            SymbolKind::Interface
        );
        assert_eq!(
            "interface".parse::<SymbolKind>().unwrap(),
            SymbolKind::Interface
        );
    }
}
