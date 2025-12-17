//! Output formatting for CLI commands

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Output context for consistent formatting across commands
///
/// This is the single source of truth for output formatting.
/// All commands should use this context for output.
#[derive(Debug, Clone)]
pub struct OutputContext {
    /// Project root for relative path calculation
    root: PathBuf,
}

impl OutputContext {
    /// Create a new output context
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Get the project root
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Convert an absolute path to relative (if within project root)
    pub fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }

    /// Check if a path is within the project root
    pub fn is_project_path(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// Print a successful response
    pub fn print_success<T: Serialize>(&self, data: T) {
        let response = serde_json::json!({
            "success": true,
            "data": data
        });
        print_json(&response);
    }

    /// Print a successful response with flat structure (data fields at top level)
    pub fn print_success_flat<T: Serialize>(&self, data: T) {
        let mut response = serde_json::to_value(data).unwrap_or(serde_json::json!({}));
        if let Some(obj) = response.as_object_mut() {
            obj.insert("success".to_string(), serde_json::json!(true));
        }
        print_json(&response);
    }

    /// Print an error response
    pub fn print_error(&self, message: &str) {
        let response = serde_json::json!({
            "success": false,
            "error": message
        });
        print_json(&response);
    }
}

fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Failed to serialize output: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_relative_path() {
        let ctx = OutputContext::new(PathBuf::from("/project"));

        assert_eq!(
            ctx.relative_path(Path::new("/project/src/main.rs")),
            "src/main.rs"
        );

        // Path outside project stays absolute
        assert_eq!(
            ctx.relative_path(Path::new("/other/file.rs")),
            "/other/file.rs"
        );
    }

    #[test]
    fn test_is_project_path() {
        let ctx = OutputContext::new(PathBuf::from("/project"));

        assert!(ctx.is_project_path(Path::new("/project/src/main.rs")));
        assert!(!ctx.is_project_path(Path::new("/other/file.rs")));
    }
}
