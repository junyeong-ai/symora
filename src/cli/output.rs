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
    /// Newline-delimited JSON; one record per line for streaming.
    Jsonl,
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

    /// Called once per handled command failure (`print_error`), so an
    /// adapter capturing output can report success/failure truthfully
    /// without re-parsing the emitted JSON.
    fn record_error(&self) {}
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
    errored: Arc<std::sync::atomic::AtomicBool>,
}

impl BufferedSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().expect("buffered sink poisoned"))
    }

    /// True when the captured command reported a handled failure.
    pub fn errored(&self) -> bool {
        self.errored.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl OutputSink for BufferedSink {
    fn write_line(&self, line: &str) {
        self.lines
            .lock()
            .expect("buffered sink poisoned")
            .push(line.to_string());
    }

    fn record_error(&self) {
        self.errored
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct OutputContext {
    root: PathBuf,
    options: OutputOptions,
    sink: Arc<dyn OutputSink>,
}

impl std::fmt::Debug for OutputContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputContext")
            .field("root", &self.root)
            .field("options", &self.options)
            .field("sink", &"<dyn OutputSink>")
            .finish()
    }
}

impl OutputContext {
    pub fn new(root: PathBuf, options: OutputOptions) -> Self {
        Self {
            root,
            options,
            sink: Arc::new(StdoutSink),
        }
    }

    pub fn with_sink(root: PathBuf, options: OutputOptions, sink: Arc<dyn OutputSink>) -> Self {
        Self {
            root,
            options,
            sink,
        }
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
        let response = match serde_json::to_value(data) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to serialize response data: {e}");
                serde_json::json!({})
            }
        };
        self.emit(&response);
    }

    pub fn print_error<E: Into<OutputError>>(&self, error: E) {
        let err: OutputError = error.into();
        self.sink.record_error();
        let response = serde_json::json!({ "error": err });
        self.emit(&response);
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
            OutputFormat::Compact | OutputFormat::Jsonl => serde_json::to_string(value),
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
}
