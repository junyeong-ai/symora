//! Output formatting for CLI commands

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Output options from CLI flags
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputOptions {
    /// Compact output for AI tools (single-line JSON, minimal tokens)
    pub compact: bool,
    /// Quiet mode (suppress success output, errors only)
    pub quiet: bool,
}

/// Output context for consistent formatting across commands
#[derive(Debug, Clone)]
pub struct OutputContext {
    root: PathBuf,
    options: OutputOptions,
}

impl OutputContext {
    pub fn new(root: PathBuf, options: OutputOptions) -> Self {
        Self { root, options }
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

    pub fn print_success<T: Serialize>(&self, data: T) {
        if self.options.quiet {
            return;
        }
        let response = serde_json::json!({
            "success": true,
            "data": data
        });
        self.print_json(&response);
    }

    pub fn print_success_flat<T: Serialize>(&self, data: T) {
        if self.options.quiet {
            return;
        }
        let mut response = serde_json::to_value(data).unwrap_or(serde_json::json!({}));
        if let Some(obj) = response.as_object_mut() {
            obj.insert("success".to_string(), serde_json::json!(true));
        }
        self.print_json(&response);
    }

    pub fn print_error(&self, message: &str) {
        let response = serde_json::json!({
            "success": false,
            "error": message
        });
        self.print_json(&response);
    }

    fn print_json(&self, value: &serde_json::Value) {
        let result = if self.options.compact {
            serde_json::to_string(value)
        } else {
            serde_json::to_string_pretty(value)
        };

        match result {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("Failed to serialize output: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relative_path() {
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
    fn test_is_project_path() {
        let ctx = OutputContext::new(PathBuf::from("/project"), OutputOptions::default());

        assert!(ctx.is_project_path(Path::new("/project/src/main.rs")));
        assert!(!ctx.is_project_path(Path::new("/other/file.rs")));
    }
}
