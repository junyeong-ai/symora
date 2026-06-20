//! Per-language LSP `initializationOptions` payloads.
//!
//! Each language's options live in its own submodule so adding or tweaking
//! one server is a single-file change. The dispatcher below maps a
//! `Language` to its payload — `None` means "send no initializationOptions"
//! (e.g., clangd configures itself via command-line args).

use std::path::Path;

use serde_json::Value;

use crate::models::symbol::Language;

mod csharp;
mod discovery;
mod exclude;
mod fsharp;
mod go;
mod java;
mod kotlin;
mod lua;
mod php;
mod python;
mod ruby;
mod rust;
mod scala;
mod small;
mod typescript;

pub fn init_options(language: Language, root_path: &Path) -> Option<Value> {
    match language {
        Language::Kotlin => Some(kotlin::kotlin_init_options(root_path)),
        Language::TypeScript | Language::JavaScript => Some(typescript::typescript_init_options()),
        Language::Python => Some(python::python_init_options(root_path)),
        Language::Rust => Some(rust::rust_init_options()),
        Language::Java => Some(java::java_init_options(root_path)),
        Language::Go => Some(go::go_init_options(root_path)),
        Language::CSharp => Some(csharp::csharp_init_options(root_path)),
        // clangd doesn't use initializationOptions — configured via command-line args.
        Language::Cpp => None,
        Language::Ruby => Some(ruby::ruby_init_options(root_path)),
        Language::PHP => Some(php::php_init_options(root_path)),
        Language::Lua => Some(lua::lua_init_options()),
        Language::Scala => Some(scala::scala_init_options()),
        Language::FSharp => Some(fsharp::fsharp_init_options(root_path)),
        Language::Elixir => Some(small::elixir_init_options()),
        Language::Haskell => Some(small::haskell_init_options()),
        Language::Dart => Some(small::dart_init_options()),
        Language::Nix => Some(small::nix_init_options()),
        Language::Yaml => Some(small::yaml_init_options()),
        Language::Terraform => Some(small::terraform_init_options()),
        Language::Zig => Some(small::zig_init_options()),
        Language::Clojure => Some(small::clojure_init_options()),
        Language::Elm => Some(small::elm_init_options()),
        Language::Erlang => Some(small::erlang_init_options()),
        Language::Swift => Some(small::swift_init_options()),
        Language::OCaml => Some(small::ocaml_init_options()),
        Language::Bash => Some(small::bash_init_options()),
        Language::Vue => Some(small::vue_init_options()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn kotlin_options_carry_indexing_and_diagnostics() {
        let root = PathBuf::from("/test/project");
        let opts = kotlin::kotlin_init_options(&root);
        assert!(opts.get("indexing").is_some());
        assert_eq!(opts["diagnostics"]["level"], 4);
        assert_eq!(opts["indexing"]["enabled"], true);
    }

    #[test]
    fn typescript_options_advertise_host_info() {
        let opts = typescript::typescript_init_options();
        assert_eq!(opts["hostInfo"], "symora");
    }

    #[test]
    fn lua_options_use_luajit_and_skip_third_party() {
        let opts = lua::lua_init_options();
        assert_eq!(opts["runtime"]["version"], "LuaJIT");
        assert_eq!(opts["workspace"]["checkThirdParty"], false);
    }

    #[test]
    fn elixir_options_target_test_env_with_dialyzer() {
        let opts = small::elixir_init_options();
        assert_eq!(opts["mixEnv"], "test");
        assert_eq!(opts["dialyzerEnabled"], true);
    }

    #[test]
    fn scala_options_disable_status_bar() {
        let opts = scala::scala_init_options();
        assert_eq!(opts["bloopSbtAlreadyInstalled"], false);
        assert_eq!(opts["statusBarProvider"], "off");
    }

    #[test]
    fn haskell_options_pick_ormolu_formatter() {
        let opts = small::haskell_init_options();
        assert_eq!(opts["haskell"]["checkProject"], false);
        assert_eq!(opts["haskell"]["formattingProvider"], "ormolu");
    }

    #[test]
    fn dart_options_enable_closing_labels() {
        let opts = small::dart_init_options();
        assert_eq!(opts["closingLabels"], true);
        assert_eq!(opts["documentation"], "full");
    }

    #[test]
    fn nix_options_carry_nixpkgs_expr() {
        let opts = small::nix_init_options();
        assert!(opts["nixpkgs"]["expr"].is_string());
    }

    #[test]
    fn yaml_options_validate_with_schema_store() {
        let opts = small::yaml_init_options();
        assert_eq!(opts["yaml"]["validate"], true);
        assert_eq!(opts["yaml"]["schemaStore"]["enable"], true);
    }

    #[test]
    fn terraform_options_enable_enhanced_validation() {
        let opts = small::terraform_init_options();
        assert_eq!(
            opts["terraform"]["validation"]["enableEnhancedValidation"],
            true
        );
    }

    #[test]
    fn zig_options_enable_semantic_tokens_and_autofix() {
        let opts = small::zig_init_options();
        assert_eq!(opts["enable_semantic_tokens"], true);
        assert_eq!(opts["enable_autofix"], true);
    }

    #[test]
    fn fsharp_options_enable_workspace_init_and_linter() {
        let root = PathBuf::from("/test/project");
        let opts = fsharp::fsharp_init_options(&root);
        assert_eq!(opts["automaticWorkspaceInit"], true);
        assert_eq!(opts["linter"], true);
    }

    #[test]
    fn dispatcher_returns_some_for_every_supported_language() {
        let root = PathBuf::from("/test");
        let supported = [
            Language::Kotlin,
            Language::TypeScript,
            Language::JavaScript,
            Language::Python,
            Language::Rust,
            Language::Java,
            Language::Go,
            Language::CSharp,
            Language::Ruby,
            Language::PHP,
            Language::Lua,
            Language::Elixir,
            Language::Scala,
            Language::Haskell,
            Language::Dart,
            Language::Nix,
            Language::Yaml,
            Language::Terraform,
            Language::Zig,
            Language::Clojure,
            Language::Elm,
            Language::Erlang,
            Language::FSharp,
            Language::Swift,
            Language::OCaml,
            Language::Vue,
            Language::Bash,
        ];
        for lang in supported {
            assert!(
                init_options(lang, &root).is_some(),
                "expected init options for {lang:?}",
            );
        }
        assert!(init_options(Language::Unknown, &root).is_none());
        // clangd is configured via CLI, not initializationOptions.
        assert!(init_options(Language::Cpp, &root).is_none());
    }
}
