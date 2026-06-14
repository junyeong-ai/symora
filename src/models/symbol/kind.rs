use std::fmt;
use std::str::FromStr;

/// Defines [`SymbolKind`] together with every projection that must agree on
/// the wire spelling of a variant: serde, [`Display`], [`FromStr`], the
/// `ALL` slice, and the canonical-name lookup. Listing a variant once here
/// makes the four projections impossible to drift apart — adding a variant
/// is a single edit the compiler then forces to completion (the
/// `canonical_name` match is exhaustive).
macro_rules! symbol_kinds {
    (
        kinds { $( $variant:ident => $name:literal ),+ $(,)? }
        aliases { $( $alias:literal => $target:ident ),* $(,)? }
    ) => {
        /// Closely mirrors LSP `SymbolKind` so the model layer can pass
        /// through LSP responses with no lossy translation.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum SymbolKind {
            $( #[serde(rename = $name)] $variant ),+
        }

        impl SymbolKind {
            /// Every variant, in declaration order — the basis for round-trip
            /// tests and the CLI name list.
            pub const ALL: &'static [SymbolKind] = &[ $( SymbolKind::$variant ),+ ];

            /// The single wire spelling of this kind, shared by serde,
            /// `Display`, and `FromStr`.
            const fn canonical_name(self) -> &'static str {
                match self { $( SymbolKind::$variant => $name ),+ }
            }
        }

        impl fmt::Display for SymbolKind {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.canonical_name())
            }
        }

        impl FromStr for SymbolKind {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s.to_lowercase().as_str() {
                    $( $name => Ok(SymbolKind::$variant), )+
                    $( $alias => Ok(SymbolKind::$target), )*
                    _ => Err(format!("Unknown symbol kind: {s}")),
                }
            }
        }
    };
}

symbol_kinds! {
    kinds {
        File => "file",
        Module => "module",
        Namespace => "namespace",
        Package => "package",
        Class => "class",
        Method => "method",
        Property => "property",
        Field => "field",
        Constructor => "constructor",
        Enum => "enum",
        Interface => "interface",
        Function => "function",
        Variable => "variable",
        Constant => "constant",
        String => "string",
        Number => "number",
        Boolean => "boolean",
        Array => "array",
        Object => "object",
        Key => "key",
        Null => "null",
        EnumMember => "enum_member",
        Struct => "struct",
        Event => "event",
        Operator => "operator",
        TypeParameter => "type_parameter",
    }
    aliases {
        // rust-analyzer reports traits as Interface.
        "trait" => Interface,
        "enummember" => EnumMember,
        "typeparameter" => TypeParameter,
    }
}

impl SymbolKind {
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

    /// A module, namespace, or package: it organizes code but does not qualify
    /// a symbol's addressing path. A method is keyed `Type/method` and a
    /// module-level item bare, matching the LSP workspace-symbol container
    /// (which never reports an enclosing module), so a `name_path` round-trips
    /// across the index, documentSymbol, and workspace surfaces.
    pub fn is_namespace_like(&self) -> bool {
        matches!(self, Self::Module | Self::Namespace | Self::Package)
    }

    pub fn parse_or_default(s: &str) -> Self {
        s.parse().unwrap_or(Self::Variable)
    }

    /// The canonical name of every kind, for CLI help and error messages.
    /// These are the documented spellings; [`FromStr`] additionally accepts
    /// the aliases declared alongside them.
    pub fn all_kind_names() -> Vec<&'static str> {
        Self::ALL.iter().map(|k| k.canonical_name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_display_and_from_str() {
        for &kind in SymbolKind::ALL {
            let text = kind.to_string();
            let parsed = text.parse::<SymbolKind>().unwrap_or_else(|e| {
                panic!("Display emitted {text:?} but FromStr rejected it: {e}")
            });
            assert_eq!(parsed, kind, "round-trip mismatch for {text:?}");
        }
    }

    #[test]
    fn every_kind_round_trips_through_serde() {
        for &kind in SymbolKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            let back: SymbolKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind, "serde round-trip mismatch for {json}");
        }
    }

    #[test]
    fn serde_and_display_share_one_spelling() {
        // The JSON spelling (serde) and the stored/CLI spelling (Display)
        // must be identical, or the DB and the API would disagree.
        for &kind in SymbolKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{kind}\""));
        }
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

    #[test]
    fn parse_or_default_falls_back_to_variable() {
        assert_eq!(
            SymbolKind::parse_or_default("not_a_kind"),
            SymbolKind::Variable
        );
        assert_eq!(SymbolKind::parse_or_default("struct"), SymbolKind::Struct);
    }
}
