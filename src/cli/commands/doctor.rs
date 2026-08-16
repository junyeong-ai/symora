use std::collections::BTreeMap;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use std::sync::Arc;

use futures::future::join_all;

use crate::app::App;
use crate::cli::OutputError;
use crate::config::LspRuntimeConfig;
use crate::infra::lsp::health::serves_workspace;
use crate::infra::lsp::servers::{self, Platform, ServerSource, check_all_servers};
use crate::models::symbol::Language;
use crate::services::store::SymbolExtractor;

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Check only specific language
    #[arg(value_name = "LANGUAGE")]
    pub language: Option<String>,
}

#[derive(Serialize)]
struct DoctorOutput {
    platform: String,
    languages: Vec<LanguageStatus>,
    summary: Summary,
    /// Config problems affecting this report: rejected [lsp.servers]
    /// stanzas — non-canonical keys, unknown fields, mistyped values —
    /// (recorded at load, never applied) and/or a whole-config load
    /// failure (the report then reflects builtin defaults).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_errors: Vec<String>,
}

#[derive(Serialize)]
struct LanguageStatus {
    language: String,
    server: String,
    /// An executable resolves at the effective command. This is a fact
    /// about the filesystem: a version-manager shim, a wrapper that cannot
    /// launch, and a working server all satisfy it alike.
    installed: bool,
    /// Whether the server actually serves this workspace, verified through
    /// the LSP handshake wherever the version probe could not settle it.
    /// An agent branches on this, never on `installed` alone.
    serves: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    tier: String,
    /// Static build facts, independent of server install state: whether the
    /// compiled-in index extractor and the tree-sitter AST grammar cover
    /// this language. Always emitted — `false` is the load-bearing value.
    symbol_extraction: bool,
    ast_search: bool,
    /// Some("config") iff an [lsp.servers] override applies — what the
    /// next server start will use. Omitted for builtin servers; that
    /// absence is how an agent detects an override that did not apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// Effective spawn command; present iff `source` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    install: Option<String>,
}

#[derive(Serialize)]
struct Summary {
    total: usize,
    /// Servers verified to serve this workspace — what an agent counts on,
    /// as distinct from how many binaries happen to be present.
    serving: usize,
    missing: usize,
}

pub async fn execute(args: DoctorArgs, app: &App) -> Result<()> {
    let ctx = &app.output;

    // Re-load through the same ConfigService every command uses, so the
    // report reflects on-disk config and can disclose load failures that
    // App::new falls back from silently.
    let (overrides, config_errors) = match app.config_service.load(false).await {
        Ok(config) => {
            let errors = config
                .unknown_keys
                .iter()
                .cloned()
                .chain(
                    config
                        .lsp
                        .server_override_errors
                        .iter()
                        .map(ToString::to_string),
                )
                .collect();
            (config.lsp.servers, errors)
        }
        Err(e) => (BTreeMap::new(), vec![e.to_string()]),
    };
    // Resolved through the same parser every `--lang` uses, so one
    // vocabulary addresses a language across the whole CLI. A substring
    // match reached both too far and not far enough: `java` also answered
    // for javascript, while `ts` answered for nothing.
    let requested = match args.language.as_deref().map(str::parse::<Language>) {
        Some(Ok(language)) => Some(language),
        Some(Err(_)) => {
            ctx.print_error(
                OutputError::invalid(format!(
                    "Unknown language: {}",
                    args.language.unwrap_or_default()
                ))
                .with_hint("Run `symora doctor` with no argument to list every language id."),
            );
            return Ok(());
        }
        None => None,
    };

    // Narrow before probing: a single-language report must not pay for
    // spawning every other language's server.
    let all_servers: Vec<_> = check_all_servers(servers::merged(&overrides))
        .into_iter()
        .filter(|s| requested.is_none_or(|language| s.language == language))
        .collect();

    // Whether a server serves is settled one way, by the handshake, for
    // every server alike. A version flag is not a substitute: several
    // builtin entries spawn a general-purpose runtime that loads the server
    // from a separately-installed package, and the runtime answers
    // `--version` whether or not that package is there.
    //
    // Handshakes are independent, and a server that never answers costs its
    // whole tier budget — run them together so one unresponsive server
    // delays the report by its own timeout rather than by everyone else's.
    let runtime = Arc::new(LspRuntimeConfig::from(app.config()));
    let serving = join_all(all_servers.iter().map(|server| {
        let runtime = Arc::clone(&runtime);
        async move {
            server.installed
                && serves_workspace(
                    server.language,
                    &server.command,
                    &server.args,
                    app.root(),
                    runtime,
                    server.init_timeout,
                )
                .await
        }
    }))
    .await;

    let languages: Vec<LanguageStatus> = all_servers
        .into_iter()
        .zip(serving)
        .map(|(s, serves)| {
            let overridden = s.source == ServerSource::Config;
            let install = if serves {
                None
            } else if s.installed {
                Some(format!(
                    "`{}` resolves but does not serve this workspace — run it directly to \
                     see why (a version-manager shim that cannot dispatch, or a missing \
                     toolchain the server needs), then run `symora daemon restart`",
                    s.command
                ))
            } else if overridden {
                Some(format!(
                    "fix [lsp.servers.{}]: command `{}` not found or not executable — \
                     correct the path or remove the override to fall back to the builtin \
                     server",
                    s.language, s.command
                ))
            } else {
                Some(s.install_instruction.to_string())
            };
            LanguageStatus {
                language: s.language.to_string(),
                server: s.name.to_string(),
                installed: s.installed,
                serves,
                version: s.version,
                tier: s.tier.as_str().to_string(),
                symbol_extraction: SymbolExtractor::is_supported(s.language),
                ast_search: crate::infra::ast::is_supported(s.language),
                source: overridden.then(|| "config".to_string()),
                command: overridden.then_some(s.command),
                install,
            }
        })
        .collect();

    let serving = languages.iter().filter(|l| l.serves).count();
    let total = languages.len();

    let response = DoctorOutput {
        platform: platform_to_string(Platform::current()),
        summary: Summary {
            total,
            serving,
            missing: total - serving,
        },
        languages,
        config_errors,
    };

    ctx.print_success(response);
    Ok(())
}

fn platform_to_string(platform: Platform) -> String {
    match platform {
        Platform::MacOS => "macos",
        Platform::Linux => "linux",
        Platform::Windows => "windows",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::symbol::Language;

    #[test]
    fn language_rows_always_carry_the_capability_booleans() {
        // Ruby has a tree-sitter AST grammar but no index extractor — the
        // row must say both, with `false` emitted rather than omitted.
        let status = LanguageStatus {
            language: "ruby".to_string(),
            server: "ruby-lsp".to_string(),
            installed: false,
            serves: false,
            version: None,
            tier: "fast".to_string(),
            symbol_extraction: SymbolExtractor::is_supported(Language::Ruby),
            ast_search: crate::infra::ast::is_supported(Language::Ruby),
            source: None,
            command: None,
            install: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["symbol_extraction"], false);
        assert_eq!(value["ast_search"], true);
    }

    /// A binary resolving and a server working are different facts, and the
    /// row states both: a version-manager shim satisfies the first while
    /// failing the second, and an agent that read only `installed` would
    /// plan a whole session around a language it cannot navigate.
    #[test]
    fn a_resolved_binary_that_does_not_serve_is_reported_as_such() {
        let status = LanguageStatus {
            language: "rust".to_string(),
            server: "rust-analyzer".to_string(),
            installed: true,
            serves: false,
            version: None,
            tier: "fast".to_string(),
            symbol_extraction: true,
            ast_search: true,
            source: None,
            command: None,
            install: Some("`rust-analyzer` resolves but does not serve".to_string()),
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["installed"], true);
        assert_eq!(value["serves"], false);
        assert!(value["install"].is_string());
    }
}
