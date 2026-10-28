//! Pull diagnostics (LSP 3.17 `textDocument/diagnostic`): the documents a
//! server answers pulls for, and what a pull says about a document's current
//! content.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;

use super::LspClient;
use crate::error::LspError;
use crate::infra::lsp::protocol::{
    DiagnosticOptions, DocumentDiagnosticReport, DocumentFilter, GlobPattern, LspDiagnostic,
    Registration, ServerCapabilities, error_codes,
};
use crate::models::lsp::uri_to_path;

pub(super) const DIAGNOSTIC_METHOD: &str = "textDocument/diagnostic";
const PROJECT_METHOD: &str = "experimental/openCargoToml";

/// What one pull said about a document's current content.
#[derive(Debug)]
pub enum DiagnosticPull {
    /// No pull provider serves the document; the server publishes its
    /// diagnostics instead.
    NotServed,
    /// The server's diagnostics for the content it holds for the document.
    Answered(Vec<LspDiagnostic>),
    /// No answer for now: the server reports its workspace loading or not
    /// healthy, cancelled the pull to be asked again, or did not answer
    /// before the request timed out.
    Pending,
    /// The server answers for the document without analyzing it: it lies
    /// outside every project the server analyzes.
    Unanalyzed,
}

/// The pull providers of one server: the one it declared at initialize and
/// those it registered since, by registration id.
#[derive(Default)]
pub(super) struct PullProviders {
    declared: Option<Provider>,
    registered: HashMap<String, Provider>,
    /// Whether the server names the project a document belongs to
    /// (rust-analyzer's `experimental/openCargoToml`).
    names_projects: bool,
}

pub(super) struct Provider {
    identifier: Option<String>,
    /// `None` serves every document.
    selector: Option<Vec<Filter>>,
}

struct Filter {
    language: Option<String>,
    scheme: Option<String>,
    pattern: Option<Pattern>,
}

struct Pattern {
    /// Set for a relative pattern, which matches paths below this base.
    base: Option<PathBuf>,
    glob: GlobMatcher,
}

impl PullProviders {
    /// Take what the server declared at initialize. A declared provider that
    /// cannot be read is no provider: the server is then read from its
    /// publishes. One declared with an id can be unregistered by it.
    pub(super) fn declare(&mut self, capabilities: &ServerCapabilities) {
        if let Some(options) = &capabilities.diagnostic_provider {
            match Provider::read(options) {
                Ok((Some(id), provider)) => {
                    self.registered.insert(id, provider);
                }
                Ok((None, provider)) => self.declared = Some(provider),
                Err(error) => tracing::warn!("Ignoring declared pull diagnostics: {error}"),
            }
        }
        self.names_projects = capabilities
            .experimental
            .as_ref()
            .and_then(|experimental| experimental.get("openCargoToml"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }

    /// Read a batch of registrations without changing any provider, so a
    /// batch is applied whole or not at all.
    pub(super) fn read(registrations: &[Registration]) -> Result<Vec<(String, Provider)>, String> {
        registrations
            .iter()
            .map(|registration| {
                let options = registration.register_options.clone().unwrap_or_default();
                Provider::read(&options)
                    .map(|(_, provider)| (registration.id.clone(), provider))
                    .map_err(|error| {
                        format!(
                            "Invalid diagnostic registration {}: {error}",
                            registration.id
                        )
                    })
            })
            .collect()
    }

    pub(super) fn register(&mut self, providers: Vec<(String, Provider)>) {
        self.registered.extend(providers);
    }

    pub(super) fn unregister(&mut self, id: &str) {
        self.registered.remove(id);
    }

    /// One pull per distinct identifier among the providers serving the
    /// document; empty when none serves it.
    fn identifiers(&self, language_id: &str, path: &Path) -> BTreeSet<Option<String>> {
        self.declared
            .iter()
            .chain(self.registered.values())
            .filter(|provider| provider.serves(language_id, path))
            .map(|provider| provider.identifier.clone())
            .collect()
    }
}

impl Provider {
    /// The provider and the id it was declared with, if any. A notebook
    /// cell filter selects no text document, so it is left out.
    fn read(options: &Value) -> Result<(Option<String>, Self), String> {
        let options: DiagnosticOptions =
            serde_json::from_value(options.clone()).map_err(|error| error.to_string())?;
        let selector = options
            .document_selector
            .map(|filters| {
                filters
                    .into_iter()
                    .filter(|filter| filter.notebook.is_none())
                    .map(Filter::read)
                    .collect()
            })
            .transpose()?;
        let provider = Self {
            identifier: options.identifier,
            selector,
        };
        Ok((options.id, provider))
    }

    fn serves(&self, language_id: &str, path: &Path) -> bool {
        self.selector.as_ref().is_none_or(|filters| {
            filters
                .iter()
                .any(|filter| filter.selects(language_id, path))
        })
    }
}

impl Filter {
    fn read(filter: DocumentFilter) -> Result<Self, String> {
        let pattern = filter
            .pattern
            .map(|pattern| {
                let (base, pattern) = match pattern {
                    GlobPattern::Absolute(pattern) => (None, pattern),
                    GlobPattern::Relative(relative) => {
                        (Some(uri_to_path(relative.base_uri.uri())), relative.pattern)
                    }
                };
                GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .build()
                    .map(|glob| Pattern {
                        base,
                        glob: glob.compile_matcher(),
                    })
                    .map_err(|error| format!("Invalid document pattern {pattern:?}: {error}"))
            })
            .transpose()?;
        Ok(Self {
            language: filter.language,
            scheme: filter.scheme,
            pattern,
        })
    }

    fn selects(&self, language_id: &str, path: &Path) -> bool {
        if self.language.is_none() && self.scheme.is_none() && self.pattern.is_none() {
            return false;
        }
        self.language
            .as_deref()
            .is_none_or(|language| language == language_id)
            && self.scheme.as_deref().is_none_or(|scheme| scheme == "file")
            && self
                .pattern
                .as_ref()
                .is_none_or(|pattern| match &pattern.base {
                    Some(base) => path
                        .strip_prefix(base)
                        .is_ok_and(|relative| pattern.glob.is_match(relative)),
                    None => pattern.glob.is_match(path),
                })
    }
}

impl LspClient {
    /// Pull `uri`'s diagnostics from every provider serving it. The server
    /// answers for the content it holds, so the document is synced first, and
    /// once it has read that content, so each answer is awaited as any
    /// request's is.
    ///
    /// A server that reports workspace status answers a pull with an empty
    /// report while it loads, and while its crate graph misses a target it
    /// reports only as unhealthy (rust-analyzer), so its answer counts only
    /// when it reported the workspace ready across the whole pull.
    /// rust-analyzer gives the same empty report for a document it does not
    /// analyze, a dependency's source or a file outside its workspace, so an
    /// empty answer from a server that names a document's project counts
    /// only for a document in one.
    pub async fn pull_diagnostics(&self, uri: &str) -> Result<DiagnosticPull, LspError> {
        let path = uri_to_path(uri);
        let (identifiers, names_projects) = {
            let providers = self.pull_providers.lock().expect("pull providers poisoned");
            (
                providers.identifiers(self.language.document_id(&path), &path),
                providers.names_projects,
            )
        };
        if identifiers.is_empty() {
            return Ok(DiagnosticPull::NotServed);
        }
        let before = self.indexing_status();
        if before.reports_not_ready() {
            return Ok(DiagnosticPull::Pending);
        }
        let document = serde_json::json!({ "textDocument": { "uri": uri } });
        let mut items = Vec::new();
        for identifier in identifiers {
            let mut params = document.clone();
            if let Some(identifier) = identifier {
                params["identifier"] = Value::String(identifier);
            }
            match self
                .request::<DocumentDiagnosticReport>(DIAGNOSTIC_METHOD, Some(params))
                .await
            {
                Ok(DocumentDiagnosticReport::Full { items: answered }) => items.extend(answered),
                Ok(DocumentDiagnosticReport::Unchanged) => {
                    return Err(LspError::Protocol(
                        "an unchanged diagnostic report answered a pull that named no previous \
                         result"
                            .to_string(),
                    ));
                }
                Err(LspError::Timeout(_)) => return Ok(DiagnosticPull::Pending),
                Err(error) if error.error_code() == error_codes::SERVER_CANCELLED => {
                    return Ok(DiagnosticPull::Pending);
                }
                Err(error) => return Err(error),
            }
        }
        let after = self.indexing_status();
        if after.reports_not_ready() || after.revision != before.revision {
            return Ok(DiagnosticPull::Pending);
        }
        if items.is_empty() && names_projects {
            match self
                .request::<Option<Value>>(PROJECT_METHOD, Some(document))
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(DiagnosticPull::Unanalyzed),
                Err(LspError::Timeout(_)) => return Ok(DiagnosticPull::Pending),
                Err(error) => return Err(error),
            }
        }
        Ok(DiagnosticPull::Answered(items))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::infra::lsp::protocol::FileChangeType;
    use crate::infra::lsp::watch::{FileChange, PathKind};
    use crate::models::symbol::Language;

    fn registration(id: &str, options: Value) -> Registration {
        Registration {
            id: id.to_string(),
            method: DIAGNOSTIC_METHOD.to_string(),
            register_options: Some(options),
        }
    }

    fn serving(providers: &PullProviders, language_id: &str, path: &str) -> Vec<Option<String>> {
        providers
            .identifiers(language_id, Path::new(path))
            .into_iter()
            .collect()
    }

    #[test]
    fn a_registration_serves_the_documents_its_selector_selects() {
        let mut providers = PullProviders::default();
        let read = PullProviders::read(&[
            registration("all", serde_json::json!({ "documentSelector": null })),
            registration(
                "swift",
                serde_json::json!({
                    "identifier": "swift",
                    "documentSelector": [{ "language": "swift" }],
                }),
            ),
            registration(
                "generated",
                serde_json::json!({
                    "identifier": "generated",
                    "documentSelector": [{ "scheme": "file", "pattern": "**/gen/*.py" }],
                }),
            ),
            registration(
                "notebook",
                serde_json::json!({
                    "identifier": "notebook",
                    "documentSelector": [{ "notebook": "jupyter-notebook", "language": "python" }],
                }),
            ),
        ])
        .unwrap();
        providers.register(read);

        assert_eq!(serving(&providers, "python", "/w/a.py"), [None]);
        assert_eq!(
            serving(&providers, "swift", "/w/a.swift"),
            [None, Some("swift".to_string())]
        );
        assert_eq!(
            serving(&providers, "python", "/w/gen/a.py"),
            [None, Some("generated".to_string())]
        );

        providers.unregister("all");
        assert!(serving(&providers, "python", "/w/a.py").is_empty());
    }

    #[test]
    fn a_declared_provider_serves_until_unregistered_by_its_id() {
        let declared = |options: Value| {
            let mut providers = PullProviders::default();
            providers.declare(&ServerCapabilities {
                diagnostic_provider: Some(options),
                ..Default::default()
            });
            providers
        };
        assert!(serving(&declared(serde_json::json!(true)), "rust", "/w/lib.rs").is_empty());
        let providers = declared(serde_json::json!({ "identifier": "rust-analyzer" }));
        assert_eq!(
            serving(&providers, "rust", "/w/lib.rs"),
            [Some("rust-analyzer".to_string())]
        );
        let mut providers = declared(serde_json::json!({ "id": "static" }));
        assert_eq!(serving(&providers, "rust", "/w/lib.rs"), [None]);
        providers.unregister("static");
        assert!(serving(&providers, "rust", "/w/lib.rs").is_empty());
    }

    #[test]
    fn a_registration_batch_applies_whole_and_unregisters_by_method() {
        let client = LspClient::new(
            Language::Python,
            PathBuf::from("/w"),
            Arc::new(crate::config::LspRuntimeConfig::default()),
            true,
        );
        let watcher = |glob: &str| {
            serde_json::json!({
                "id": "watch",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": { "watchers": [{ "globPattern": glob }] },
            })
        };
        let pull = |pattern: &str| {
            serde_json::json!({
                "id": "pull",
                "method": DIAGNOSTIC_METHOD,
                "registerOptions": { "documentSelector": [{ "pattern": pattern }] },
            })
        };
        let batch =
            |registrations: [Value; 2]| Some(serde_json::json!({ "registrations": registrations }));
        let watched = || {
            let change = FileChange {
                path: PathBuf::from("/w/a.py"),
                change: FileChangeType::Changed,
                kind: PathKind::File,
            };
            !client
                .watched_files
                .lock()
                .unwrap()
                .select(&[change])
                .is_empty()
        };
        let pulled = || {
            !client
                .pull_providers
                .lock()
                .unwrap()
                .identifiers("python", Path::new("/w/a.py"))
                .is_empty()
        };

        for refused in [
            batch([watcher("**/*.py"), pull("[")]),
            batch([watcher("["), pull("**/*.py")]),
        ] {
            assert!(client.register_capabilities(&refused).is_err());
            assert!(!watched() && !pulled());
        }
        client
            .register_capabilities(&batch([watcher("**/*.py"), pull("**/*.py")]))
            .unwrap();
        assert!(watched() && pulled());
        client
            .unregister_capabilities(&Some(serde_json::json!({
                "unregisterations": [{ "id": "pull", "method": DIAGNOSTIC_METHOD }],
            })))
            .unwrap();
        assert!(watched() && !pulled());
    }
}
