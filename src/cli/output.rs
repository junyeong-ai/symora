use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::ValueEnum;
use serde::Serialize;

use super::errors::OutputError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Pretty-printed JSON (default; human-readable).
    #[default]
    Pretty,
    /// Minified single-line JSON (token-efficient for AI agents).
    Compact,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OutputOptions {
    pub format: OutputFormat,
    pub quiet: bool,
    pub token_estimate: bool,
}

/// Sink the formatted JSON line is written to.
///
/// Defaults to stdout for the CLI; the MCP adapter swaps in a buffered
/// sink so it can capture and forward responses without spawning a
/// subprocess.
pub trait OutputSink: Send + Sync {
    fn write_line(&self, line: &str);
}

pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn write_line(&self, line: &str) {
        println!("{line}");
    }
}

#[derive(Default, Clone)]
pub struct BufferedSink {
    lines: Arc<Mutex<Vec<String>>>,
}

impl BufferedSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().expect("buffered sink poisoned"))
    }
}

impl OutputSink for BufferedSink {
    fn write_line(&self, line: &str) {
        self.lines
            .lock()
            .expect("buffered sink poisoned")
            .push(line.to_string());
    }
}

#[derive(Clone)]
pub struct OutputContext {
    root: PathBuf,
    options: OutputOptions,
    sink: Arc<dyn OutputSink>,
    /// Per-response char ceiling enforced in `print_success`; 0 = off.
    /// Constructors leave it off — `App` is the single application point,
    /// above the daemon/direct mode boundary, so every surface agrees.
    max_response_chars: usize,
    /// Set once when a handler reports a handled failure via `print_error`.
    /// Lifted here (above the sink) so the bare CLI and the MCP adapter read
    /// the SAME failure signal — the one that drives both the process exit
    /// code and the MCP `isError` flag. The production read path is the
    /// `errored_flag()` handle (an `Arc::clone`), checked after the owning `App`
    /// has moved into the command future; the derived `Clone` sharing the same
    /// `Arc` is a consistency property, not that read path.
    errored: Arc<std::sync::atomic::AtomicBool>,
    /// Whole-config load failures captured once at `App` init (a malformed
    /// `.symora/config.toml` that `App` fell back from to defaults). Surfaced as
    /// a top-level `config_errors` array on whatever command the user actually
    /// ran — omitted when empty, so the common case is untouched — instead of
    /// the failure being visible only if they think to run `doctor`. Set above
    /// the daemon/direct mode boundary, so both modes disclose identically.
    config_errors: Vec<String>,
}

impl std::fmt::Debug for OutputContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputContext")
            .field("root", &self.root)
            .field("options", &self.options)
            .field("sink", &"<dyn OutputSink>")
            .field("max_response_chars", &self.max_response_chars)
            .field("errored", &self.errored())
            .finish()
    }
}

impl OutputContext {
    pub fn new(root: PathBuf, options: OutputOptions) -> Self {
        Self {
            root,
            options,
            sink: Arc::new(StdoutSink),
            max_response_chars: 0,
            errored: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config_errors: Vec::new(),
        }
    }

    pub fn with_sink(root: PathBuf, options: OutputOptions, sink: Arc<dyn OutputSink>) -> Self {
        Self {
            root,
            options,
            sink,
            max_response_chars: 0,
            errored: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config_errors: Vec::new(),
        }
    }

    /// Carry whole-config load failures so every command discloses them. Set
    /// once by `App` at init; empty in the common (clean-config) case.
    pub fn with_config_errors(mut self, config_errors: Vec<String>) -> Self {
        self.config_errors = config_errors;
        self
    }

    /// The captured whole-config load failures, so a re-homed context (the MCP
    /// adapter's `with_output_sink`) keeps disclosing them across the clone.
    pub(crate) fn config_errors_snapshot(&self) -> Vec<String> {
        self.config_errors.clone()
    }

    pub fn with_max_response_chars(mut self, max_chars: usize) -> Self {
        self.max_response_chars = max_chars;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }

    pub fn is_project_path(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    pub fn format_path(path: &Path, root: &Path) -> String {
        path.strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }

    pub fn print_success<T: Serialize>(&self, data: T) {
        if self.options.quiet {
            return;
        }
        let mut response = match serde_json::to_value(data) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to serialize response data: {e}");
                serde_json::json!({})
            }
        };
        // Inject BEFORE fitting so the size ceiling accounts for the disclosure
        // (the contract: the char ceiling applies last and wins on the emitted
        // total). The fitter only ever drops whole `Section` items — never this
        // top-level `config_errors` array — so a load failure rides along and is
        // never dropped to fit; the fitter trims result items to stay under
        // budget instead.
        self.inject_config_errors(&mut response);
        if self.max_response_chars > 0 {
            // The ceiling guards the exact string emitted in the active
            // format — host caps apply to emitted characters, so pretty
            // output spends its budget on indentation by design.
            let measure = |value: &serde_json::Value| -> usize {
                match self.options.format {
                    OutputFormat::Pretty => serde_json::to_string_pretty(value),
                    OutputFormat::Compact => serde_json::to_string(value),
                }
                .map(|s| s.chars().count())
                .unwrap_or(usize::MAX)
            };
            if crate::cli::response::fit_to_char_budget(
                &mut response,
                self.max_response_chars,
                &measure,
            ) {
                tracing::debug!("response fitted to output.max_response_chars");
            }
        }
        self.emit(&response);
    }

    pub fn print_error<E: Into<OutputError>>(&self, error: E) {
        let err: OutputError = error.into();
        // Categorical: the flag is keyed on the print_error path itself, never
        // on inspecting emitted JSON. A successful command that reports
        // diagnostics or an embedded `Section` error as DATA (via
        // print_success) is not a command failure and leaves this false.
        self.errored
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut response = serde_json::json!({ "error": err });
        self.inject_config_errors(&mut response);
        self.emit(&response);
    }

    /// Surface captured whole-config load failures as a top-level
    /// `config_errors` array. Injected only when there are failures and the
    /// command did not already emit its own `config_errors` (doctor / config
    /// show own that key), so it never clobbers a command's report and stays
    /// absent in the clean-config common case.
    fn inject_config_errors(&self, response: &mut serde_json::Value) {
        if self.config_errors.is_empty() {
            return;
        }
        if let Some(obj) = response.as_object_mut()
            && !obj.contains_key("config_errors")
        {
            obj.insert(
                "config_errors".to_string(),
                serde_json::json!(self.config_errors),
            );
        }
    }

    /// True once any handler reported a handled failure via `print_error`.
    /// The single predicate behind the CLI process exit code and the MCP
    /// `isError` flag.
    pub fn errored(&self) -> bool {
        self.errored.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A shared handle to the failure flag, for a caller that must read it
    /// after moving the owning context (the MCP capture moves its scoped
    /// `App` into the command future, then reports `isError` from this).
    pub(crate) fn errored_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.errored)
    }

    /// Emit raw text output (used by markdown / plain-text shapes that bypass
    /// the JSON contract — pack's `--shape markdown` is the canonical case).
    /// `quiet` still suppresses output; `token-estimate` reports against the
    /// full text on stderr.
    pub fn print_text(&self, text: &str) {
        if self.options.quiet {
            return;
        }
        for line in text.split_inclusive('\n') {
            self.sink.write_line(line.trim_end_matches('\n'));
        }
        if self.options.token_estimate {
            eprintln!(
                "[symora token-estimate≈{}]",
                crate::utils::estimate_tokens(text)
            );
        }
    }

    fn emit(&self, value: &serde_json::Value) {
        let serialized = match self.options.format {
            OutputFormat::Pretty => serde_json::to_string_pretty(value),
            OutputFormat::Compact => serde_json::to_string(value),
        };

        match serialized {
            Ok(json) => {
                self.sink.write_line(&json);
                if self.options.token_estimate {
                    eprintln!(
                        "[symora token-estimate≈{}]",
                        crate::utils::estimate_tokens(&json)
                    );
                }
            }
            Err(e) => eprintln!("Failed to serialize output: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_strips_root_prefix() {
        let ctx = OutputContext::new(PathBuf::from("/project"), OutputOptions::default());
        assert_eq!(
            ctx.relative_path(Path::new("/project/src/main.rs")),
            "src/main.rs"
        );
        assert_eq!(
            ctx.relative_path(Path::new("/other/file.rs")),
            "/other/file.rs"
        );
    }

    #[test]
    fn is_project_path_checks_root() {
        let ctx = OutputContext::new(PathBuf::from("/project"), OutputOptions::default());
        assert!(ctx.is_project_path(Path::new("/project/src/main.rs")));
        assert!(!ctx.is_project_path(Path::new("/other/file.rs")));
    }

    #[test]
    fn output_format_default_is_pretty() {
        assert_eq!(OutputFormat::default(), OutputFormat::Pretty);
    }

    #[test]
    fn buffered_sink_captures_print_success() {
        let buf = BufferedSink::new();
        let ctx = OutputContext::with_sink(
            PathBuf::from("/project"),
            OutputOptions {
                format: OutputFormat::Compact,
                ..Default::default()
            },
            Arc::new(buf.clone()),
        );
        ctx.print_success(serde_json::json!({ "ok": true }));
        let captured = buf.take();
        assert_eq!(captured, vec![r#"{"ok":true}"#.to_string()]);
    }

    #[test]
    fn buffered_sink_captures_structured_error() {
        let buf = BufferedSink::new();
        let ctx = OutputContext::with_sink(
            PathBuf::from("/project"),
            OutputOptions {
                format: OutputFormat::Compact,
                ..Default::default()
            },
            Arc::new(buf.clone()),
        );
        ctx.print_error(OutputError::not_found("missing"));
        let captured = buf.take();
        assert_eq!(captured.len(), 1);
        assert!(captured[0].contains("not_found"));
        assert!(captured[0].contains("missing"));
    }

    #[test]
    fn config_load_errors_surface_on_every_command_but_omit_when_clean() {
        // Clean config: no field, so the common case forces no defensive parse.
        let clean = OutputContext::new(PathBuf::from("/project"), OutputOptions::default());
        let mut v = serde_json::json!({ "count": 0 });
        clean.inject_config_errors(&mut v);
        assert!(v.get("config_errors").is_none());

        // A captured load failure surfaces on an arbitrary command's output...
        let buf = BufferedSink::new();
        let ctx = OutputContext::with_sink(
            PathBuf::from("/project"),
            OutputOptions {
                format: OutputFormat::Compact,
                ..Default::default()
            },
            Arc::new(buf.clone()),
        )
        .with_config_errors(vec!["invalid TOML at line 3".to_string()]);
        ctx.print_success(serde_json::json!({ "count": 0, "items": [] }));
        let out = buf.take();
        assert!(out[0].contains(r#""config_errors":["invalid TOML at line 3"]"#));

        // ...and on a failing command's error envelope too.
        ctx.print_error(OutputError::not_found("x"));
        let out = buf.take();
        assert!(out[0].contains("config_errors"));
        assert!(out[0].contains("\"error\""));
    }

    #[test]
    fn config_errors_never_clobber_a_command_that_owns_the_key() {
        // doctor / config show emit their own config_errors; the output layer
        // must not overwrite them with the startup-load set.
        let ctx = OutputContext::new(PathBuf::from("/project"), OutputOptions::default())
            .with_config_errors(vec!["startup load failed".to_string()]);
        let mut v = serde_json::json!({ "config_errors": ["stanza rejected"] });
        ctx.inject_config_errors(&mut v);
        assert_eq!(v["config_errors"], serde_json::json!(["stanza rejected"]));
    }

    #[test]
    fn quiet_mode_suppresses_success() {
        let buf = BufferedSink::new();
        let ctx = OutputContext::with_sink(
            PathBuf::from("/project"),
            OutputOptions {
                quiet: true,
                ..Default::default()
            },
            Arc::new(buf.clone()),
        );
        ctx.print_success(serde_json::json!({ "ok": true }));
        assert!(buf.take().is_empty());
    }

    // The failure flag is the single predicate behind the CLI exit code and
    // the MCP isError flag. It is keyed on the print_error path, never on
    // output content.

    #[test]
    fn print_error_sets_errored_print_success_does_not() {
        let ctx = OutputContext::new(PathBuf::from("/project"), OutputOptions::default());
        assert!(!ctx.errored());
        ctx.print_success(serde_json::json!({ "ok": true }));
        assert!(!ctx.errored(), "a successful response is not a failure");
        ctx.print_error(OutputError::not_found("missing".to_string()));
        assert!(ctx.errored());
    }

    #[test]
    fn quiet_mode_still_records_and_emits_errors() {
        // --quiet is "errors only": it suppresses success bodies but an error
        // is still emitted AND still flips the flag (so CI gets exit 2).
        let buf = BufferedSink::new();
        let ctx = OutputContext::with_sink(
            PathBuf::from("/project"),
            OutputOptions {
                quiet: true,
                ..Default::default()
            },
            Arc::new(buf.clone()),
        );
        ctx.print_error(OutputError::not_found("missing".to_string()));
        assert!(ctx.errored());
        assert!(!buf.take().is_empty(), "errors are shown under --quiet");
    }

    #[test]
    fn errored_flag_is_shared_across_clones() {
        // The MCP capture reads the flag after the scoped context is moved;
        // a clone must observe a failure set on the original (shared Arc).
        let ctx = OutputContext::new(PathBuf::from("/project"), OutputOptions::default());
        let flag = ctx.errored_flag();
        let clone = ctx.clone();
        ctx.print_error(OutputError::not_found("missing".to_string()));
        assert!(clone.errored());
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    }
}
