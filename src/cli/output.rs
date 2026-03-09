use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Copy, Default)]
pub struct OutputOptions {
    pub compact: bool,
    pub quiet: bool,
}
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
        self.print_json(&response);
    }

    pub fn print_error(&self, message: &str) {
        let response = serde_json::json!({
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
