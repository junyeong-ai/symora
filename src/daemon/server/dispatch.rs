use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon::params::{FileParams, PositionParams, ProjectParams};
use crate::daemon::protocol::{Request, RpcError, methods};
use crate::daemon::wire;
use crate::error::LspError;
use crate::services::lsp::LspService;

use super::config::DaemonRuntimeConfig;
use super::context::{ProjectContext, ProjectsMap, get_context};
use super::handlers;
use super::store_handlers;

const NON_LSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

pub(super) async fn dispatch(
    request: Request,
    projects: &ProjectsMap,
    config: &DaemonRuntimeConfig,
    start_time: Instant,
) -> Result<serde_json::Value, RpcError> {
    match request.method.as_str() {
        // `version` lets the client detect a daemon left over from a
        // different binary and restart it before any wire exchange, so the
        // wire format never needs cross-version compatibility.
        methods::PING => Ok(serde_json::json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
            "build": crate::daemon::protocol::BUILD_ID,
        })),
        methods::SHUTDOWN => Ok(serde_json::json!({"shutting_down": true})),
        methods::STATUS => tokio::time::timeout(
            NON_LSP_REQUEST_TIMEOUT,
            handlers::handle_status(projects, config, start_time),
        )
        .await
        .map_err(timeout_error)?,
        _ => {
            let params = request.params.unwrap_or(serde_json::json!({}));
            let p: ProjectParams = parse_params(&params)?;
            let ctx = get_context(projects, &p.project).await?;
            let timeout = estimate_request_timeout(&request.method, &params, &ctx);
            tokio::time::timeout(
                timeout,
                dispatch_project(&request.method, &params, Arc::clone(&ctx)),
            )
            .await
            .map_err(timeout_error)?
        }
    }
}

fn timeout_error(_: tokio::time::error::Elapsed) -> RpcError {
    RpcError::internal_error("Request timed out")
}

fn estimate_request_timeout(
    method: &str,
    params: &serde_json::Value,
    ctx: &ProjectContext,
) -> Duration {
    crate::daemon::lsp_request_timeout(&ctx.config, method, params)
        .unwrap_or(NON_LSP_REQUEST_TIMEOUT)
}

async fn dispatch_project(
    method: &str,
    params: &serde_json::Value,
    ctx: Arc<ProjectContext>,
) -> Result<serde_json::Value, RpcError> {
    match method {
        // Symbol operations
        methods::FIND_SYMBOLS => handlers::handle_find_symbols(params, &ctx).await,
        methods::WORKSPACE_SYMBOLS => handlers::handle_workspace_symbols(params, &ctx).await,

        // Position-based operations
        methods::FIND_REFERENCES => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::ReferencesResponse::from(
                    ctx.lsp.find_references(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::GOTO_DEFINITION => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::DefinitionResponse::from_definition(
                    ctx.lsp.goto_definition(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::GOTO_TYPE_DEFINITION => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::DefinitionResponse::from_type_definition(
                    ctx.lsp.goto_type_definition(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::FIND_IMPLEMENTATIONS => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::ImplementationsResponse::from(
                    ctx.lsp.find_implementations(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::HOVER => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::HoverResponse::from_hover(
                    ctx.lsp.hover(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::SIGNATURE_HELP => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::SignatureResponse::from_help(
                    ctx.lsp.signature_help(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::INCOMING_CALLS => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::CallsResponse::from(
                    ctx.lsp.incoming_calls(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::OUTGOING_CALLS => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::CallsResponse::from(
                    ctx.lsp.outgoing_calls(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::SUPERTYPES => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::TypeHierarchyResponse::from(
                    ctx.lsp.supertypes(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::SUBTYPES => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::TypeHierarchyResponse::from(
                    ctx.lsp.subtypes(&f, l, c).await?,
                ))
            })
            .await
        }

        methods::PREPARE_RENAME => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                let result = ctx.lsp.prepare_rename(&f, l, c).await?;
                to_json(wire::PrepareRenameResponse {
                    placeholder: result.map(|r| r.placeholder),
                })
            })
            .await
        }

        methods::CODE_ACTIONS => {
            handle_position(params, Arc::clone(&ctx), |ctx, f, l, c| async move {
                to_json(wire::CodeActionsResponse::from_actions(
                    ctx.lsp.code_actions(&f, l, c).await?,
                ))
            })
            .await
        }

        // File-based operations
        methods::DIAGNOSTICS => {
            handle_file(params, Arc::clone(&ctx), |ctx, f| async move {
                to_json(wire::DiagnosticsResponse::from(
                    ctx.lsp.diagnostics(&f).await?,
                ))
            })
            .await
        }

        methods::FOLDING_RANGES => {
            handle_file(params, Arc::clone(&ctx), |ctx, f| async move {
                to_json(wire::FoldingRangesResponse::from(
                    ctx.lsp.folding_ranges(&f).await?,
                ))
            })
            .await
        }

        methods::CODE_LENSES => {
            handle_file(params, Arc::clone(&ctx), |ctx, f| async move {
                to_json(wire::CodeLensResponse::from(ctx.lsp.code_lenses(&f).await?))
            })
            .await
        }

        methods::FORMAT => {
            handle_file(params, Arc::clone(&ctx), |ctx, f| async move {
                to_json(wire::FormatResponse::from(ctx.lsp.format(&f).await?))
            })
            .await
        }

        // Special operations
        methods::RENAME => handlers::handle_rename(params, &ctx).await,
        methods::INLAY_HINTS => handlers::handle_inlay_hints(params, &ctx).await,
        methods::SELECTION_RANGES => handlers::handle_selection_ranges(params, &ctx).await,
        methods::APPLY_CODE_ACTION => handlers::handle_apply_action(params, &ctx).await,

        // Language status
        methods::LANGUAGE_STATUS => handlers::handle_language_status(params, &ctx).await,

        // Post-edit notes
        methods::NOTE_FILES_EDITED => handlers::handle_note_files_edited(params, &ctx).await,

        // Store operations
        methods::REFRESH_FILES => store_handlers::handle_refresh_files(params, &ctx).await,
        methods::SEARCH_SYMBOLS => store_handlers::handle_search_symbols(params, &ctx).await,
        methods::SEARCH_CONTENT => store_handlers::handle_search_content(params, &ctx).await,
        methods::INDEX_BUILD => store_handlers::handle_index_build(params, &ctx).await,
        methods::INDEX_STATUS => store_handlers::handle_index_status(&ctx).await,
        methods::INDEX_IS_CURRENT => store_handlers::handle_index_is_current(&ctx).await,
        methods::INDEXED_LANGUAGES => store_handlers::handle_indexed_languages(&ctx).await,
        methods::INDEX_CLEAR => store_handlers::handle_index_clear(&ctx).await,

        _ => Err(RpcError::method_not_found(method)),
    }
}

pub(super) fn parse_params<T: DeserializeOwned>(params: &serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|e| RpcError::invalid_params(&e.to_string()))
}

pub(super) fn to_json<T: Serialize>(value: T) -> Result<serde_json::Value, LspError> {
    serde_json::to_value(value).map_err(|e| LspError::Protocol(e.to_string()))
}

async fn handle_position<F, Fut>(
    params: &serde_json::Value,
    ctx: Arc<ProjectContext>,
    handler: F,
) -> Result<serde_json::Value, RpcError>
where
    F: FnOnce(Arc<ProjectContext>, PathBuf, u32, u32) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, LspError>>,
{
    let p: PositionParams = parse_params(params)?;
    handler(ctx, PathBuf::from(p.file), p.line, p.column)
        .await
        .map_err(RpcError::from)
}

async fn handle_file<F, Fut>(
    params: &serde_json::Value,
    ctx: Arc<ProjectContext>,
    handler: F,
) -> Result<serde_json::Value, RpcError>
where
    F: FnOnce(Arc<ProjectContext>, PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, LspError>>,
{
    let p: FileParams = parse_params(params)?;
    handler(ctx, PathBuf::from(p.file))
        .await
        .map_err(RpcError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    fn project(timeout: u64) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".symora")).unwrap();
        std::fs::write(
            root.path().join(".symora/config.toml"),
            format!("[lsp]\ntimeout_secs = {timeout}\n"),
        )
        .unwrap();
        root
    }

    #[tokio::test]
    async fn request_bounds_use_each_retained_project_config() {
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let first = project(10);
        let second = project(100);
        let params = serde_json::json!({"file": "main.go"});
        for (root, seconds) in [(&first, 10), (&second, 100)] {
            let ctx = get_context(&projects, root.path().to_str().unwrap())
                .await
                .unwrap();
            assert_eq!(
                estimate_request_timeout(methods::HOVER, &params, &ctx),
                Duration::from_secs(seconds)
            );
            std::fs::write(
                root.path().join(".symora/config.toml"),
                "[lsp]\ntimeout_secs = 1\n",
            )
            .unwrap();
            let retained = get_context(&projects, root.path().to_str().unwrap())
                .await
                .unwrap();
            assert!(Arc::ptr_eq(&ctx, &retained));
            assert_eq!(
                estimate_request_timeout(methods::HOVER, &params, &retained),
                Duration::from_secs(seconds)
            );
        }
        assert_eq!(projects.read().await.len(), 2);
    }

    #[tokio::test]
    async fn workspace_symbols_uses_python_on_client_and_server() {
        let root = project(60);
        let ctx = ProjectContext::new(root.path());
        let params = serde_json::json!({"language": "python", "query": "Name"});
        assert_eq!(
            estimate_request_timeout(methods::WORKSPACE_SYMBOLS, &params, &ctx),
            Duration::from_secs(2880)
        );
        assert_eq!(
            crate::daemon::client::calculate_timeout(
                &ctx.config,
                &params,
                methods::WORKSPACE_SYMBOLS
            ),
            Duration::from_secs(2880)
        );
    }

    #[tokio::test]
    async fn file_language_precedes_language_and_unmapped_bounds_stay_fixed() {
        let root = project(10);
        let ctx = ProjectContext::new(root.path());
        let params = serde_json::json!({"file": "main.go", "language": "python"});
        assert_eq!(
            estimate_request_timeout(methods::WORKSPACE_SYMBOLS, &params, &ctx),
            Duration::from_secs(60)
        );
        assert_eq!(
            crate::daemon::client::calculate_timeout(
                &ctx.config,
                &params,
                methods::WORKSPACE_SYMBOLS
            ),
            Duration::from_secs(60)
        );
        for method in [
            methods::PING,
            methods::STATUS,
            methods::SHUTDOWN,
            methods::REFRESH_FILES,
            methods::NOTE_FILES_EDITED,
            methods::LANGUAGE_STATUS,
            methods::SEARCH_SYMBOLS,
            methods::SEARCH_CONTENT,
            methods::INDEX_BUILD,
            methods::INDEX_STATUS,
            methods::INDEX_IS_CURRENT,
            methods::INDEXED_LANGUAGES,
            methods::INDEX_CLEAR,
        ] {
            assert_eq!(
                estimate_request_timeout(method, &params, &ctx),
                Duration::from_secs(600),
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn project_request_is_counted_once_in_status() {
        let root = project(60);
        let config = DaemonRuntimeConfig::load().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        let params = serde_json::json!({"project": root.path(), "files": []});
        let start = Instant::now();
        for count in 1..=2 {
            let result = dispatch(
                Request::new(count, methods::NOTE_FILES_EDITED, Some(params.clone())),
                &projects,
                &config,
                start,
            )
            .await
            .unwrap();
            assert_eq!(result["noted"], true);
            let status = dispatch(
                Request::new(3, methods::STATUS, None),
                &projects,
                &config,
                start,
            )
            .await
            .unwrap();
            assert_eq!(status["active_projects"], 1);
            assert_eq!(
                status["projects"][0]["project"],
                root.path().to_str().unwrap()
            );
            assert_eq!(status["projects"][0]["requests"], count);
        }
    }

    #[test]
    fn project_lease_lives_until_response_or_cancellation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            for cancel in [false, true] {
                let root = project(60);
                let config = DaemonRuntimeConfig::load().unwrap();
                let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
                let (release, wait) = std::sync::mpsc::channel();
                let (started, ready) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                });
                ready.recv().unwrap();
                let mut request = Box::pin(dispatch(
                    Request::new(
                        1,
                        methods::INDEX_CLEAR,
                        Some(serde_json::json!({"project": root.path()})),
                    ),
                    &projects,
                    &config,
                    Instant::now(),
                ));
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(request.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let ctx = Arc::clone(projects.read().await.get(root.path()).unwrap());
                tokio::time::sleep(Duration::from_millis(2)).await;
                assert!(!ctx.is_idle(Duration::ZERO));
                if cancel {
                    drop(request);
                    release.send(()).unwrap();
                } else {
                    release.send(()).unwrap();
                    assert_eq!(request.await.unwrap()["cleared"], true);
                }
                blocker.await.unwrap();
                tokio::time::sleep(Duration::from_millis(2)).await;
                assert!(ctx.is_idle(Duration::ZERO));
            }
        });
    }

    #[tokio::test]
    async fn system_requests_create_no_project_context() {
        let root = project(60);
        let config = DaemonRuntimeConfig::load().unwrap();
        let projects: ProjectsMap = Arc::new(RwLock::new(HashMap::new()));
        for method in [methods::PING, methods::STATUS, methods::SHUTDOWN] {
            for params in [None, Some(serde_json::json!({"project": root.path()}))] {
                dispatch(
                    Request::new(1, method, params),
                    &projects,
                    &config,
                    Instant::now(),
                )
                .await
                .unwrap();
                assert!(projects.read().await.is_empty(), "{method}");
            }
        }
    }
}
