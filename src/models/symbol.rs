//! Symbol model definitions
//!
//! Core types for representing code symbols from LSP.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Represents a code symbol from LSP
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    pub kind: SymbolKind,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Symbol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overload_idx: Option<u32>,
}

impl Symbol {
    pub fn new(name: String, kind: SymbolKind, location: Location) -> Self {
        Self {
            name,
            name_path: None,
            kind,
            location,
            container: None,
            body: None,
            children: Vec::new(),
            overload_idx: None,
        }
    }

    pub fn with_container(mut self, container: impl Into<String>) -> Self {
        self.container = Some(container.into());
        self
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn with_children(mut self, children: Vec<Symbol>) -> Self {
        self.children = children;
        self
    }

    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }

    pub fn compute_paths(&mut self, parent_path: Option<&str>) {
        let base_path = match parent_path {
            Some(parent) => format!("{}/{}", parent, self.name),
            None => self.name.clone(),
        };

        self.name_path = Some(match self.overload_idx {
            Some(idx) => format!("{}[{}]", base_path, idx),
            None => base_path.clone(),
        });

        for child in &mut self.children {
            child.compute_paths(Some(&base_path));
        }
    }

    /// Compute paths for a list of top-level symbols (includes overload detection)
    pub fn compute_paths_for_all(symbols: &mut [Symbol]) {
        Self::assign_overload_indices(symbols);
        for symbol in symbols {
            symbol.compute_paths(None);
        }
    }

    fn assign_overload_indices(symbols: &mut [Symbol]) {
        use std::collections::HashMap;

        let mut name_counts: HashMap<String, u32> = HashMap::new();
        for symbol in symbols.iter() {
            *name_counts.entry(symbol.name.clone()).or_insert(0) += 1;
        }

        let mut name_indices: HashMap<String, u32> = HashMap::new();
        for symbol in symbols.iter_mut() {
            let count = name_counts.get(&symbol.name).copied().unwrap_or(1);
            if count > 1 {
                let idx = name_indices.entry(symbol.name.clone()).or_insert(0);
                symbol.overload_idx = Some(*idx);
                *idx += 1;
            }

            if !symbol.children.is_empty() {
                Self::assign_overload_indices(&mut symbol.children);
            }
        }
    }

    /// Get the name_path or fall back to name
    pub fn path(&self) -> &str {
        self.name_path.as_deref().unwrap_or(&self.name)
    }

    /// Check if this symbol matches a path pattern
    /// Supports:
    /// - Simple name: "method"
    /// - Relative path: "Class/method" (matches as suffix)
    /// - Absolute path: "/Class/method" (exact match from root)
    /// - Wildcards: "*/method", "Class/*"
    /// - Overload index: "method[0]", "Class/method[1]"
    pub fn matches_path(&self, pattern: &str) -> bool {
        let path = self.path();

        // Absolute path matching (starts with /)
        if let Some(abs_pattern) = pattern.strip_prefix('/') {
            return Self::matches_pattern(path, abs_pattern, true);
        }

        Self::matches_pattern(path, pattern, false)
    }

    fn matches_pattern(path: &str, pattern: &str, exact: bool) -> bool {
        // Handle overload index in pattern
        let (pattern_base, pattern_idx) = Self::parse_overload_index(pattern);
        let (path_base, path_idx) = Self::parse_overload_index(path);

        // If pattern specifies an index, it must match
        if let Some(pidx) = pattern_idx
            && path_idx != Some(pidx)
        {
            return false;
        }

        let pattern = pattern_base;
        let path = path_base;

        if pattern.contains('*') {
            Self::matches_wildcard(path, pattern, exact)
        } else if exact {
            // Absolute matching requires exact path equality
            path == pattern
        } else if pattern.contains('/') {
            // Relative path matching allows suffix
            path == pattern || path.ends_with(&format!("/{}", pattern))
        } else {
            // Simple name match - extract last component
            let name = path.rsplit('/').next().unwrap_or(path);
            name == pattern
        }
    }

    fn parse_overload_index(s: &str) -> (&str, Option<u32>) {
        if let Some(bracket_pos) = s.rfind('[')
            && s.ends_with(']')
            && let Ok(idx) = s[bracket_pos + 1..s.len() - 1].parse::<u32>()
        {
            return (&s[..bracket_pos], Some(idx));
        }
        (s, None)
    }

    fn matches_wildcard(path: &str, pattern: &str, exact: bool) -> bool {
        let parts: Vec<&str> = pattern.split('/').collect();
        let path_parts: Vec<&str> = path.split('/').collect();

        if exact && parts.len() != path_parts.len() {
            return false;
        }

        if parts.len() > path_parts.len() {
            return false;
        }

        let offset = path_parts.len() - parts.len();
        for (i, part) in parts.iter().enumerate() {
            let path_part = match path_parts.get(offset + i) {
                Some(p) => *p,
                None => return false,
            };

            if !Self::matches_glob_part(path_part, part) {
                return false;
            }
        }
        true
    }

    fn matches_glob_part(value: &str, pattern: &str) -> bool {
        if pattern == "*" {
            return true;
        }

        if let Some(prefix) = pattern.strip_suffix('*') {
            return value.starts_with(prefix);
        }

        if let Some(suffix) = pattern.strip_prefix('*') {
            return value.ends_with(suffix);
        }

        if let Some((prefix, suffix)) = pattern.split_once('*') {
            return value.starts_with(prefix) && value.ends_with(suffix);
        }

        value == pattern
    }

    /// Filter symbols by path pattern, searching recursively
    pub fn filter_by_path(symbols: &[Symbol], pattern: &str) -> Vec<Symbol> {
        let mut results = Vec::new();
        Self::collect_matching(symbols, pattern, &mut results);
        results
    }

    fn collect_matching(symbols: &[Symbol], pattern: &str, results: &mut Vec<Symbol>) {
        for symbol in symbols {
            if symbol.matches_path(pattern) {
                results.push(symbol.clone());
            }
            Self::collect_matching(&symbol.children, pattern, results);
        }
    }

    pub fn find_by_path<'a>(symbols: &'a [Symbol], path: &str) -> Option<&'a Symbol> {
        for symbol in symbols {
            if symbol.path() == path {
                return Some(symbol);
            }
            if let Some(found) = Self::find_by_path(&symbol.children, path) {
                return Some(found);
            }
        }
        None
    }

    /// Check if symbol name contains substring (case-insensitive)
    pub fn matches_substring(&self, substring: &str) -> bool {
        self.name.to_lowercase().contains(&substring.to_lowercase())
    }

    /// Filter symbols with advanced criteria
    pub fn filter_advanced(
        symbols: &[Symbol],
        pattern: Option<&str>,
        substring: bool,
        include_kinds: Option<&[SymbolKind]>,
        exclude_kinds: Option<&[SymbolKind]>,
        exclude_low_level: bool,
    ) -> Vec<Symbol> {
        let mut results = Vec::new();
        Self::collect_advanced(
            symbols,
            pattern,
            substring,
            include_kinds,
            exclude_kinds,
            exclude_low_level,
            &mut results,
        );
        results
    }

    fn collect_advanced(
        symbols: &[Symbol],
        pattern: Option<&str>,
        substring: bool,
        include_kinds: Option<&[SymbolKind]>,
        exclude_kinds: Option<&[SymbolKind]>,
        exclude_low_level: bool,
        results: &mut Vec<Symbol>,
    ) {
        for symbol in symbols {
            let excluded = exclude_kinds.is_some_and(|k| k.contains(&symbol.kind))
                || include_kinds.is_some_and(|k| !k.contains(&symbol.kind))
                || (exclude_low_level && symbol.kind.is_low_level());

            if excluded {
                Self::collect_advanced(
                    &symbol.children,
                    pattern,
                    substring,
                    include_kinds,
                    exclude_kinds,
                    exclude_low_level,
                    results,
                );
                continue;
            }

            let matches = match pattern {
                None => true,
                Some(p) if substring => symbol.matches_substring(p),
                Some(p) => symbol.matches_path(p),
            };

            if matches {
                results.push(symbol.clone());
            }

            Self::collect_advanced(
                &symbol.children,
                pattern,
                substring,
                include_kinds,
                exclude_kinds,
                exclude_low_level,
                results,
            );
        }
    }

    /// Normalize symbol name - handle empty or placeholder names from LSP
    pub fn normalize_name(name: &str, file: &std::path::Path, kind: SymbolKind) -> String {
        let name = name.trim();

        // Skip normalization for valid names
        if !name.is_empty()
            && name != "<unknown>"
            && name != "<anonymous>"
            && !name.starts_with('<')
        {
            return name.to_string();
        }

        // Generate meaningful fallback name from file + kind
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("anonymous");

        let suffix = match kind {
            SymbolKind::Module => "module",
            SymbolKind::Function => "fn",
            SymbolKind::Variable | SymbolKind::Constant => "export",
            SymbolKind::Object => "config",
            _ => "symbol",
        };

        format!("{}_{}", stem, suffix)
    }

    /// Strip type parameters and parameter lists from symbol names.
    /// Java/Kotlin LSP servers return names like "myMethod(int, String) <T>" but we want "myMethod".
    /// This enables proper overload handling via overload_idx.
    pub fn strip_type_parameters(name: &str) -> String {
        let name = name.trim();

        // Remove parameter list: "method(int, String)" -> "method"
        let name = if let Some(paren_pos) = name.find('(') {
            &name[..paren_pos]
        } else {
            name
        };

        // Remove generic type parameters: "MyClass<T, K>" -> "MyClass"
        let name = if let Some(angle_pos) = name.find('<') {
            &name[..angle_pos]
        } else {
            name
        };

        name.trim().to_string()
    }
}

/// Symbol classification (aligned with LSP SymbolKind)
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
    /// Convert from LSP SymbolKind number
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
            _ => Self::Variable, // Default fallback
        }
    }

    /// Convert to LSP SymbolKind number
    pub fn to_lsp(&self) -> u32 {
        match self {
            Self::File => 1,
            Self::Module => 2,
            Self::Namespace => 3,
            Self::Package => 4,
            Self::Class => 5,
            Self::Method => 6,
            Self::Property => 7,
            Self::Field => 8,
            Self::Constructor => 9,
            Self::Enum => 10,
            Self::Interface => 11,
            Self::Function => 12,
            Self::Variable => 13,
            Self::Constant => 14,
            Self::String => 15,
            Self::Number => 16,
            Self::Boolean => 17,
            Self::Array => 18,
            Self::Object => 19,
            Self::Key => 20,
            Self::Null => 21,
            Self::EnumMember => 22,
            Self::Struct => 23,
            Self::Event => 24,
            Self::Operator => 25,
            Self::TypeParameter => 26,
        }
    }

    /// Check if this is a type definition
    pub fn is_type(&self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Struct | Self::Enum | Self::TypeParameter
        )
    }

    /// Check if this is callable
    pub fn is_callable(&self) -> bool {
        matches!(self, Self::Function | Self::Method | Self::Constructor)
    }

    /// Check if this is a low-level/data symbol (variables, constants, literals)
    /// Low-level symbols are typically implementation details rather than structure
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

    /// Check if this is a structural symbol (classes, functions, interfaces, etc.)
    pub fn is_structural(&self) -> bool {
        !self.is_low_level()
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
            // "trait" is aliased to Interface (Rust traits are reported as Interface by rust-analyzer)
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

impl SymbolKind {
    /// Parse symbol kind from string with fallback to Variable for unknown kinds
    pub fn from_str_loose(s: &str) -> Self {
        s.parse().unwrap_or(Self::Variable)
    }

    /// All valid kind names for error messages
    pub fn all_kind_names() -> &'static [&'static str] {
        &[
            "function",
            "class",
            "method",
            "field",
            "variable",
            "constant",
            "interface",
            "trait", // Alias for interface (Rust traits)
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

/// Supported programming languages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    // === Systems Languages ===
    Rust,
    Cpp,
    Zig,

    // === JVM Languages ===
    Java,
    Kotlin,
    Scala,
    Clojure,

    // === .NET Languages ===
    CSharp,
    FSharp,

    // === Web Languages ===
    TypeScript,
    JavaScript,
    Vue,

    // === Scripting Languages ===
    Python,
    Ruby,
    PHP,
    Perl,
    Lua,
    Bash,
    PowerShell,

    // === Functional Languages ===
    Haskell,
    Elixir,
    Erlang,
    Elm,
    OCaml,

    // === Mobile/Application Languages ===
    Go,
    Swift,
    Dart,

    // === Config/DevOps Languages ===
    Terraform,
    Yaml,
    Toml,
    Nix,
    Rego,

    // === Scientific Languages ===
    R,
    Julia,
    Fortran,

    // === Documentation ===
    Markdown,

    #[default]
    Unknown,
}

impl Language {
    /// Detect language from file extension
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            // Systems
            "rs" => Self::Rust,
            "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" | "hxx" => Self::Cpp,
            "zig" => Self::Zig,

            // JVM
            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            "scala" | "sc" => Self::Scala,
            "clj" | "cljs" | "cljc" | "edn" => Self::Clojure,

            // .NET
            "cs" => Self::CSharp,
            "fs" | "fsx" | "fsi" => Self::FSharp,

            // Web
            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "vue" => Self::Vue,

            // Scripting
            "py" | "pyi" => Self::Python,
            "rb" | "rake" | "gemspec" => Self::Ruby,
            "php" => Self::PHP,
            "pl" | "pm" | "t" => Self::Perl,
            "lua" => Self::Lua,
            "sh" | "bash" | "zsh" => Self::Bash,
            "ps1" | "psm1" | "psd1" => Self::PowerShell,

            // Functional
            "hs" | "lhs" => Self::Haskell,
            "ex" | "exs" => Self::Elixir,
            "erl" | "hrl" => Self::Erlang,
            "elm" => Self::Elm,
            "ml" | "mli" => Self::OCaml,

            // Mobile/Application
            "go" => Self::Go,
            "swift" => Self::Swift,
            "dart" => Self::Dart,

            // Config/DevOps
            "tf" | "tfvars" | "hcl" => Self::Terraform,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "nix" => Self::Nix,
            "rego" => Self::Rego,

            // Scientific
            "r" | "rmd" => Self::R,
            "jl" => Self::Julia,
            "f" | "f90" | "f95" | "f03" | "f08" | "for" => Self::Fortran,

            // Documentation
            "md" | "markdown" => Self::Markdown,

            _ => Self::Unknown,
        }
    }

    /// Detect language from file path
    pub fn from_path(path: &Path) -> Self {
        path.extension()
            .and_then(|e| e.to_str())
            .map(Self::from_extension)
            .unwrap_or(Self::Unknown)
    }

    /// Parse language from string, returning Unknown for unrecognized values
    pub fn from_str_loose(s: &str) -> Self {
        s.parse().unwrap_or(Self::Unknown)
    }

    /// Get file extensions for this language
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            // Systems
            Self::Rust => &["rs"],
            Self::Cpp => &["c", "cpp", "cc", "cxx", "h", "hpp", "hxx"],
            Self::Zig => &["zig"],

            // JVM
            Self::Java => &["java"],
            Self::Kotlin => &["kt", "kts"],
            Self::Scala => &["scala", "sc"],
            Self::Clojure => &["clj", "cljs", "cljc", "edn"],

            // .NET
            Self::CSharp => &["cs"],
            Self::FSharp => &["fs", "fsx", "fsi"],

            // Web
            Self::TypeScript => &["ts", "tsx", "mts", "cts"],
            Self::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Self::Vue => &["vue"],

            // Scripting
            Self::Python => &["py", "pyi"],
            Self::Ruby => &["rb", "rake", "gemspec"],
            Self::PHP => &["php"],
            Self::Perl => &["pl", "pm", "t"],
            Self::Lua => &["lua"],
            Self::Bash => &["sh", "bash", "zsh"],
            Self::PowerShell => &["ps1", "psm1", "psd1"],

            // Functional
            Self::Haskell => &["hs", "lhs"],
            Self::Elixir => &["ex", "exs"],
            Self::Erlang => &["erl", "hrl"],
            Self::Elm => &["elm"],
            Self::OCaml => &["ml", "mli"],

            // Mobile/Application
            Self::Go => &["go"],
            Self::Swift => &["swift"],
            Self::Dart => &["dart"],

            // Config/DevOps
            Self::Terraform => &["tf", "tfvars", "hcl"],
            Self::Yaml => &["yaml", "yml"],
            Self::Toml => &["toml"],
            Self::Nix => &["nix"],
            Self::Rego => &["rego"],

            // Scientific
            Self::R => &["r", "rmd"],
            Self::Julia => &["jl"],
            Self::Fortran => &["f", "f90", "f95", "f03", "f08", "for"],

            // Documentation
            Self::Markdown => &["md", "markdown"],

            Self::Unknown => &[],
        }
    }

    /// Get directories that should be ignored for this language (build artifacts, caches, etc.)
    pub fn ignored_directories(&self) -> &'static [&'static str] {
        match self {
            // Systems
            Self::Rust => &["target"],
            Self::Cpp => &["build", "cmake-build-debug", "cmake-build-release", "out"],
            Self::Zig => &["zig-out", "zig-cache"],

            // JVM
            Self::Java => &["target", "build", "bin", "out", "classes", ".gradle"],
            Self::Kotlin => &["build", "out", ".gradle", ".kotlin"],
            Self::Scala => &["target", ".bloop", ".metals", ".bsp"],
            Self::Clojure => &["target", ".cpcache", ".clj-kondo"],

            // .NET
            Self::CSharp => &["bin", "obj", "packages", ".vs"],
            Self::FSharp => &["bin", "obj", "packages", ".ionide", ".fake", "paket-files"],

            // Web
            Self::TypeScript | Self::JavaScript => &[
                "node_modules",
                "dist",
                "build",
                "coverage",
                ".next",
                ".nuxt",
            ],
            Self::Vue => &["node_modules", "dist", "build", ".nuxt"],

            // Scripting
            Self::Python => &[
                "__pycache__",
                ".venv",
                "venv",
                ".env",
                "build",
                "dist",
                ".eggs",
                ".mypy_cache",
                ".pytest_cache",
                ".pixi",
            ],
            Self::Ruby => &[
                "vendor", ".bundle", "tmp", "log", "coverage", ".yardoc", "pkg",
            ],
            Self::PHP => &["vendor", "node_modules", "cache"],
            Self::Perl => &["blib", "_build", "local"],
            Self::Lua => &[".luarocks", "lua_modules"],
            Self::Bash | Self::PowerShell => &[],

            // Functional
            Self::Haskell => &["dist", "dist-newstyle", ".stack-work", ".cabal-sandbox"],
            Self::Elixir => &["_build", "deps", ".elixir_ls"],
            Self::Erlang => &["_build", "deps", "ebin"],
            Self::Elm => &["elm-stuff"],
            Self::OCaml => &["_build", "_opam"],

            // Mobile/Application
            Self::Go => &["vendor"],
            Self::Swift => &[".build", "DerivedData", ".swiftpm"],
            Self::Dart => &[".dart_tool", "build", ".pub-cache"],

            // Config/DevOps
            Self::Terraform => &[".terraform"],
            Self::Yaml | Self::Toml => &[],
            Self::Nix => &["result", ".direnv"],
            Self::Rego => &[],

            // Scientific
            Self::R => &["renv"],
            Self::Julia => &[".julia"],
            Self::Fortran => &[],

            // Documentation
            Self::Markdown => &[],

            Self::Unknown => &[],
        }
    }

    /// Get LSP language ID
    pub fn lsp_id(&self) -> &'static str {
        match self {
            // Systems
            Self::Rust => "rust",
            Self::Cpp => "cpp",
            Self::Zig => "zig",

            // JVM
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Scala => "scala",
            Self::Clojure => "clojure",

            // .NET
            Self::CSharp => "csharp",
            Self::FSharp => "fsharp",

            // Web
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Vue => "vue",

            // Scripting
            Self::Python => "python",
            Self::Ruby => "ruby",
            Self::PHP => "php",
            Self::Perl => "perl",
            Self::Lua => "lua",
            Self::Bash => "shellscript",
            Self::PowerShell => "powershell",

            // Functional
            Self::Haskell => "haskell",
            Self::Elixir => "elixir",
            Self::Erlang => "erlang",
            Self::Elm => "elm",
            Self::OCaml => "ocaml",

            // Mobile/Application
            Self::Go => "go",
            Self::Swift => "swift",
            Self::Dart => "dart",

            // Config/DevOps
            Self::Terraform => "terraform",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Nix => "nix",
            Self::Rego => "rego",

            // Scientific
            Self::R => "r",
            Self::Julia => "julia",
            Self::Fortran => "fortran",

            // Documentation
            Self::Markdown => "markdown",

            Self::Unknown => "plaintext",
        }
    }

    /// Get all supported file extensions
    pub fn all_extensions() -> Vec<&'static str> {
        vec![
            // Systems
            "rs", "c", "cpp", "cc", "cxx", "h", "hpp", "hxx", "zig", // JVM
            "java", "kt", "kts", "scala", "sc", "clj", "cljs", "cljc", "edn", // .NET
            "cs", "fs", "fsx", "fsi", // Web
            "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "vue", // Scripting
            "py", "pyi", "rb", "rake", "gemspec", "php", "pl", "pm", "t", "lua", "sh", "bash",
            "zsh", "ps1", "psm1", "psd1", // Functional
            "hs", "lhs", "ex", "exs", "erl", "hrl", "elm", "ml", "mli",
            // Mobile/Application
            "go", "swift", "dart", // Config/DevOps
            "tf", "tfvars", "hcl", "yaml", "yml", "toml", "nix", "rego", // Scientific
            "r", "rmd", "jl", "f", "f90", "f95", "f03", "f08", "for", // Documentation
            "md", "markdown",
        ]
    }

    /// Get all supported languages (excluding Unknown)
    pub fn all() -> Vec<Self> {
        vec![
            Self::Rust,
            Self::Cpp,
            Self::Zig,
            Self::Java,
            Self::Kotlin,
            Self::Scala,
            Self::Clojure,
            Self::CSharp,
            Self::FSharp,
            Self::TypeScript,
            Self::JavaScript,
            Self::Vue,
            Self::Python,
            Self::Ruby,
            Self::PHP,
            Self::Perl,
            Self::Lua,
            Self::Bash,
            Self::PowerShell,
            Self::Haskell,
            Self::Elixir,
            Self::Erlang,
            Self::Elm,
            Self::OCaml,
            Self::Go,
            Self::Swift,
            Self::Dart,
            Self::Terraform,
            Self::Yaml,
            Self::Toml,
            Self::Nix,
            Self::Rego,
            Self::R,
            Self::Julia,
            Self::Fortran,
            Self::Markdown,
        ]
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.lsp_id())
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            // Systems
            "rust" | "rs" => Ok(Self::Rust),
            "cpp" | "c++" | "c" => Ok(Self::Cpp),
            "zig" => Ok(Self::Zig),

            // JVM
            "java" => Ok(Self::Java),
            "kotlin" | "kt" => Ok(Self::Kotlin),
            "scala" => Ok(Self::Scala),
            "clojure" | "clj" => Ok(Self::Clojure),

            // .NET
            "csharp" | "c#" | "cs" => Ok(Self::CSharp),
            "fsharp" | "f#" | "fs" => Ok(Self::FSharp),

            // Web
            "typescript" | "ts" => Ok(Self::TypeScript),
            "javascript" | "js" => Ok(Self::JavaScript),
            "vue" => Ok(Self::Vue),

            // Scripting
            "python" | "py" => Ok(Self::Python),
            "ruby" | "rb" => Ok(Self::Ruby),
            "php" => Ok(Self::PHP),
            "perl" | "pl" => Ok(Self::Perl),
            "lua" => Ok(Self::Lua),
            "bash" | "sh" | "shell" => Ok(Self::Bash),
            "powershell" | "pwsh" | "ps1" => Ok(Self::PowerShell),

            // Functional
            "haskell" | "hs" => Ok(Self::Haskell),
            "elixir" | "ex" => Ok(Self::Elixir),
            "erlang" | "erl" => Ok(Self::Erlang),
            "elm" => Ok(Self::Elm),
            "ocaml" | "ml" => Ok(Self::OCaml),

            // Mobile/Application
            "go" | "golang" => Ok(Self::Go),
            "swift" => Ok(Self::Swift),
            "dart" => Ok(Self::Dart),

            // Config/DevOps
            "terraform" | "tf" | "hcl" => Ok(Self::Terraform),
            "yaml" | "yml" => Ok(Self::Yaml),
            "toml" => Ok(Self::Toml),
            "nix" => Ok(Self::Nix),
            "rego" => Ok(Self::Rego),

            // Scientific
            "r" => Ok(Self::R),
            "julia" | "jl" => Ok(Self::Julia),
            "fortran" | "f90" => Ok(Self::Fortran),

            // Documentation
            "markdown" | "md" => Ok(Self::Markdown),

            _ => Err(format!("Unknown language: {}", s)),
        }
    }
}

/// Source code location
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    /// File path
    pub file: PathBuf,

    /// Start line (1-indexed)
    pub line: u32,

    /// Start column (1-indexed)
    pub column: u32,

    /// End line (1-indexed, optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,

    /// End column (1-indexed, optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

impl Location {
    /// Create a new location with full range
    pub fn new(file: PathBuf, line: u32, column: u32, end_line: u32, end_column: u32) -> Self {
        Self {
            file,
            line,
            column,
            end_line: Some(end_line),
            end_column: Some(end_column),
        }
    }

    /// Create location for a single position
    pub fn point(file: PathBuf, line: u32, column: u32) -> Self {
        Self {
            file,
            line,
            column,
            end_line: None,
            end_column: None,
        }
    }

    /// Create location with optional end position
    pub fn with_end(
        file: PathBuf,
        line: u32,
        column: u32,
        end_line: Option<u32>,
        end_column: Option<u32>,
    ) -> Self {
        Self {
            file,
            line,
            column,
            end_line,
            end_column,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file.display(), self.line, self.column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_symbol(name: &str, kind: SymbolKind) -> Symbol {
        Symbol::new(
            name.to_string(),
            kind,
            Location::point(PathBuf::from("test.rs"), 1, 1),
        )
    }

    #[test]
    fn test_compute_paths() {
        let mut class = make_symbol("MyClass", SymbolKind::Class);
        let method1 = make_symbol("update", SymbolKind::Method);
        let method2 = make_symbol("reset", SymbolKind::Method);
        class.children = vec![method1, method2];

        class.compute_paths(None);

        assert_eq!(class.name_path, Some("MyClass".to_string()));
        assert_eq!(
            class.children[0].name_path,
            Some("MyClass/update".to_string())
        );
        assert_eq!(
            class.children[1].name_path,
            Some("MyClass/reset".to_string())
        );
    }

    #[test]
    fn test_matches_path_exact() {
        let mut sym = make_symbol("update", SymbolKind::Method);
        sym.name_path = Some("MyClass/update".to_string());

        assert!(sym.matches_path("update"));
        assert!(sym.matches_path("MyClass/update"));
        assert!(!sym.matches_path("OtherClass/update"));
    }

    #[test]
    fn test_matches_path_wildcard() {
        let mut sym = make_symbol("update", SymbolKind::Method);
        sym.name_path = Some("MyClass/update".to_string());

        assert!(sym.matches_path("*/update"));
        assert!(sym.matches_path("MyClass/*"));
        assert!(!sym.matches_path("*/reset"));
    }

    #[test]
    fn test_filter_by_path() {
        let mut class = make_symbol("MyClass", SymbolKind::Class);
        let method1 = make_symbol("update", SymbolKind::Method);
        let method2 = make_symbol("reset", SymbolKind::Method);
        class.children = vec![method1, method2];
        class.compute_paths(None);

        let results = Symbol::filter_by_path(&[class.clone()], "MyClass/update");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "update");

        let results = Symbol::filter_by_path(&[class.clone()], "*/reset");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "reset");

        let results = Symbol::filter_by_path(&[class], "MyClass/*");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_language_from_extension() {
        assert_eq!(Language::from_extension("kt"), Language::Kotlin);
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("java"), Language::Java);
        assert_eq!(Language::from_extension("txt"), Language::Unknown);
    }

    #[test]
    fn test_symbol_kind_lsp_conversion() {
        assert_eq!(SymbolKind::from_lsp(5), SymbolKind::Class);
        assert_eq!(SymbolKind::from_lsp(12), SymbolKind::Function);
        assert_eq!(SymbolKind::Class.to_lsp(), 5);
        assert_eq!(SymbolKind::Function.to_lsp(), 12);
    }

    #[test]
    fn test_location_display() {
        let loc = Location::point(PathBuf::from("/test/file.rs"), 10, 5);
        assert_eq!(loc.to_string(), "/test/file.rs:10:5");
    }

    #[test]
    fn test_symbol_kind_is_low_level() {
        assert!(SymbolKind::Variable.is_low_level());
        assert!(SymbolKind::Constant.is_low_level());
        assert!(SymbolKind::String.is_low_level());
        assert!(SymbolKind::Number.is_low_level());
        assert!(!SymbolKind::Function.is_low_level());
        assert!(!SymbolKind::Class.is_low_level());
        assert!(!SymbolKind::Method.is_low_level());
    }

    #[test]
    fn test_symbol_kind_is_structural() {
        assert!(SymbolKind::Function.is_structural());
        assert!(SymbolKind::Class.is_structural());
        assert!(SymbolKind::Method.is_structural());
        assert!(!SymbolKind::Variable.is_structural());
        assert!(!SymbolKind::Constant.is_structural());
    }

    #[test]
    fn test_matches_substring() {
        let sym = make_symbol("getValue", SymbolKind::Function);
        assert!(sym.matches_substring("get"));
        assert!(sym.matches_substring("Value"));
        assert!(sym.matches_substring("getValue"));
        assert!(sym.matches_substring("GET")); // case-insensitive
        assert!(!sym.matches_substring("set"));
    }

    #[test]
    fn test_filter_advanced_with_kinds() {
        let mut class = make_symbol("MyClass", SymbolKind::Class);
        let method = make_symbol("update", SymbolKind::Method);
        let field = make_symbol("count", SymbolKind::Variable);
        class.children = vec![method, field];
        class.compute_paths(None);

        // Include only methods
        let include_kinds = vec![SymbolKind::Method];
        let results = Symbol::filter_advanced(
            &[class.clone()],
            None,
            false,
            Some(&include_kinds),
            None,
            false,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "update");

        // Exclude variables
        let exclude_kinds = vec![SymbolKind::Variable];
        let results = Symbol::filter_advanced(
            &[class.clone()],
            None,
            false,
            None,
            Some(&exclude_kinds),
            false,
        );
        assert_eq!(results.len(), 2); // class + method
        assert!(results.iter().all(|s| s.kind != SymbolKind::Variable));
    }

    #[test]
    fn test_filter_advanced_exclude_low_level() {
        let mut class = make_symbol("MyClass", SymbolKind::Class);
        let method = make_symbol("update", SymbolKind::Method);
        let field = make_symbol("count", SymbolKind::Variable);
        class.children = vec![method, field];
        class.compute_paths(None);

        let results = Symbol::filter_advanced(&[class], None, false, None, None, true);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|s| !s.kind.is_low_level()));
    }

    #[test]
    fn test_filter_advanced_substring() {
        let mut class = make_symbol("UserService", SymbolKind::Class);
        let m1 = make_symbol("getUser", SymbolKind::Method);
        let m2 = make_symbol("setUser", SymbolKind::Method);
        let m3 = make_symbol("deleteAll", SymbolKind::Method);
        class.children = vec![m1, m2, m3];
        class.compute_paths(None);

        let results = Symbol::filter_advanced(&[class], Some("User"), true, None, None, false);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_overload_index_assignment() {
        let mut class = make_symbol("MyClass", SymbolKind::Class);
        let m1 = make_symbol("doSomething", SymbolKind::Method);
        let m2 = make_symbol("doSomething", SymbolKind::Method);
        let m3 = make_symbol("doSomething", SymbolKind::Method);
        let m4 = make_symbol("unique", SymbolKind::Method);
        class.children = vec![m1, m2, m3, m4];

        let mut symbols = vec![class];
        Symbol::compute_paths_for_all(&mut symbols);

        let class = &symbols[0];
        assert_eq!(class.children[0].overload_idx, Some(0));
        assert_eq!(class.children[1].overload_idx, Some(1));
        assert_eq!(class.children[2].overload_idx, Some(2));
        assert_eq!(class.children[3].overload_idx, None);

        assert_eq!(
            class.children[0].name_path,
            Some("MyClass/doSomething[0]".to_string())
        );
        assert_eq!(
            class.children[1].name_path,
            Some("MyClass/doSomething[1]".to_string())
        );
        assert_eq!(
            class.children[2].name_path,
            Some("MyClass/doSomething[2]".to_string())
        );
        assert_eq!(
            class.children[3].name_path,
            Some("MyClass/unique".to_string())
        );
    }

    #[test]
    fn test_matches_path_overload_index() {
        let mut sym = make_symbol("doSomething", SymbolKind::Method);
        sym.overload_idx = Some(1);
        sym.name_path = Some("MyClass/doSomething[1]".to_string());

        assert!(sym.matches_path("doSomething"));
        assert!(sym.matches_path("doSomething[1]"));
        assert!(!sym.matches_path("doSomething[0]"));
        assert!(sym.matches_path("MyClass/doSomething[1]"));
        assert!(!sym.matches_path("MyClass/doSomething[0]"));
    }

    #[test]
    fn test_matches_path_absolute() {
        let mut sym = make_symbol("update", SymbolKind::Method);
        sym.name_path = Some("MyClass/update".to_string());

        // Relative matching (allows suffix)
        assert!(sym.matches_path("update"));
        assert!(sym.matches_path("MyClass/update"));

        // Absolute matching (exact from root)
        assert!(sym.matches_path("/MyClass/update"));
        assert!(!sym.matches_path("/update"));
        assert!(!sym.matches_path("/Other/MyClass/update"));
    }

    #[test]
    fn test_parse_overload_index() {
        assert_eq!(Symbol::parse_overload_index("method"), ("method", None));
        assert_eq!(
            Symbol::parse_overload_index("method[0]"),
            ("method", Some(0))
        );
        assert_eq!(
            Symbol::parse_overload_index("method[123]"),
            ("method", Some(123))
        );
        assert_eq!(
            Symbol::parse_overload_index("Class/method[2]"),
            ("Class/method", Some(2))
        );
        assert_eq!(
            Symbol::parse_overload_index("method[abc]"),
            ("method[abc]", None)
        );
        assert_eq!(Symbol::parse_overload_index("method["), ("method[", None));
    }
}
