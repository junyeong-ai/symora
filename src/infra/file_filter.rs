//! File filtering with .gitignore integration
//!
//! Uses the `ignore` crate (from ripgrep) for gitignore-style pattern matching.
//! Supports multiple .gitignore files with proper directory scoping.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};

/// File filter configuration
#[derive(Debug, Clone)]
pub struct FileFilterConfig {
    /// Root directory for relative path resolution
    pub root: PathBuf,
    /// Use .gitignore files for filtering
    pub respect_gitignore: bool,
    /// Use .symora/ignore file for filtering
    pub respect_symora_ignore: bool,
    /// Additional ignore patterns (gitignore syntax)
    pub ignore_patterns: Vec<String>,
    /// Include patterns (override ignores)
    pub include_patterns: Vec<String>,
    /// Hidden files/directories (starting with .)
    pub include_hidden: bool,
}

impl Default for FileFilterConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            respect_gitignore: true,
            respect_symora_ignore: true,
            ignore_patterns: Vec::new(),
            include_patterns: Vec::new(),
            include_hidden: false,
        }
    }
}

/// File filter with gitignore integration
pub struct FileFilter {
    config: FileFilterConfig,
    gitignore: Option<Gitignore>,
    symora_ignore: Option<Gitignore>,
    overrides: Option<Override>,
}

impl FileFilter {
    /// Create a new file filter with the given configuration
    pub fn new(config: FileFilterConfig) -> Self {
        let gitignore = if config.respect_gitignore {
            Self::load_gitignore(&config.root)
        } else {
            None
        };

        let symora_ignore = if config.respect_symora_ignore {
            Self::load_symora_ignore(&config.root)
        } else {
            None
        };

        let overrides = Self::build_overrides(&config);

        Self {
            config,
            gitignore,
            symora_ignore,
            overrides,
        }
    }

    /// Create a filter that respects .gitignore in the given root
    pub fn with_gitignore(root: impl AsRef<Path>) -> Self {
        Self::new(FileFilterConfig {
            root: root.as_ref().to_path_buf(),
            respect_gitignore: true,
            respect_symora_ignore: true,
            ..Default::default()
        })
    }

    /// Load all .gitignore files from root directory
    /// Uses WalkBuilder to respect gitignore while discovering nested .gitignore files
    fn load_gitignore(root: &Path) -> Option<Gitignore> {
        let mut builder = GitignoreBuilder::new(root);

        // Load root .gitignore first
        let gitignore_path = root.join(".gitignore");
        if gitignore_path.exists()
            && let Some(err) = builder.add(&gitignore_path)
        {
            tracing::warn!("Failed to parse .gitignore: {}", err);
        }

        // Use WalkBuilder to walk directories, respecting gitignore
        // This prevents walking into ignored directories like node_modules, .gradle, etc.
        let walker = WalkBuilder::new(root)
            .hidden(false) // Don't skip hidden to find nested .gitignore files
            .git_ignore(true) // Respect gitignore while walking
            .git_global(true)
            .git_exclude(true)
            .max_depth(Some(10)) // Limit depth to prevent very deep recursion
            .build();

        for entry in walker.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.file_name() == Some(std::ffi::OsStr::new(".gitignore"))
                && path != gitignore_path
                && let Some(err) = builder.add(path)
            {
                tracing::warn!("Failed to parse {:?}: {}", path, err);
            }
        }

        builder.build().ok()
    }

    /// Load .symora/ignore file
    fn load_symora_ignore(root: &Path) -> Option<Gitignore> {
        let ignore_path = root.join(".symora").join("ignore");
        if !ignore_path.exists() {
            return None;
        }

        let mut builder = GitignoreBuilder::new(root);
        if let Some(err) = builder.add(&ignore_path) {
            tracing::warn!("Failed to parse .symora/ignore: {}", err);
        }

        builder.build().ok()
    }

    /// Build override patterns from config
    fn build_overrides(config: &FileFilterConfig) -> Option<Override> {
        if config.ignore_patterns.is_empty() && config.include_patterns.is_empty() {
            return None;
        }

        let mut builder = OverrideBuilder::new(&config.root);

        // Add ignore patterns (with ! prefix for negation in Override)
        for pattern in &config.ignore_patterns {
            // Override uses inverted logic: patterns match = include
            // We want ignore patterns to exclude, so we need to use !pattern
            let negated = format!("!{}", pattern);
            if let Err(e) = builder.add(&negated) {
                tracing::warn!("Invalid ignore pattern '{}': {}", pattern, e);
            }
        }

        // Add include patterns (these override ignores)
        for pattern in &config.include_patterns {
            if let Err(e) = builder.add(pattern) {
                tracing::warn!("Invalid include pattern '{}': {}", pattern, e);
            }
        }

        builder.build().ok()
    }

    /// Check if a path should be ignored
    pub fn is_ignored(&self, path: &Path) -> bool {
        // Get relative path from root
        let relative = match path.strip_prefix(&self.config.root) {
            Ok(p) => p,
            Err(_) => path,
        };

        let is_dir = path.is_dir();

        // Check if any path component matches default ignore patterns
        for component in relative.components() {
            if let std::path::Component::Normal(name) = component
                && let Some(name_str) = name.to_str()
            {
                // Check against default ignore patterns (catches .gradle, node_modules, etc.)
                for pattern in DEFAULT_IGNORE_PATTERNS {
                    if pattern.starts_with('*') {
                        // Glob pattern (e.g., "*.log")
                        let suffix = pattern.trim_start_matches('*');
                        if name_str.ends_with(suffix) {
                            return true;
                        }
                    } else if name_str == *pattern {
                        return true;
                    }
                }
            }
        }

        // Check hidden files/directories (starting with .)
        if !self.config.include_hidden {
            for component in relative.components() {
                if let std::path::Component::Normal(name) = component
                    && let Some(s) = name.to_str()
                {
                    // Skip .symora itself since we need it for config
                    if s.starts_with('.') && s != ".symora" {
                        return true;
                    }
                }
            }
        }

        // Check .symora/ignore (highest priority for custom patterns)
        if let Some(ref ignore) = self.symora_ignore {
            match ignore.matched(relative, is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }

        // Check overrides (config patterns)
        if let Some(ref overrides) = self.overrides {
            match overrides.matched(relative, is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }

        // Check .gitignore
        if let Some(ref gitignore) = self.gitignore {
            match gitignore.matched(relative, is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }

        false
    }

    /// Check if a path should be included (inverse of is_ignored)
    pub fn should_include(&self, path: &Path) -> bool {
        !self.is_ignored(path)
    }

    /// Create a WalkBuilder configured with this filter
    pub fn walk_builder(&self) -> WalkBuilder {
        let mut builder = WalkBuilder::new(&self.config.root);

        builder
            .hidden(!self.config.include_hidden)
            .git_ignore(self.config.respect_gitignore)
            .git_global(self.config.respect_gitignore)
            .git_exclude(self.config.respect_gitignore);

        // Add custom ignore patterns
        for pattern in &self.config.ignore_patterns {
            builder.add_ignore(pattern);
        }

        builder
    }

    /// Get all files that should be indexed
    pub fn discover_files(&self, extensions: &[&str]) -> Vec<PathBuf> {
        let mut files = Vec::new();

        for entry in self.walk_builder().build().filter_map(|e| e.ok()) {
            let path = entry.path();

            // Only files
            if !path.is_file() {
                continue;
            }

            // Check extension
            if !extensions.is_empty() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !extensions.contains(&ext) {
                    continue;
                }
            }

            // Apply additional filters (symora ignore, overrides)
            if self.should_include(path) {
                files.push(path.to_path_buf());
            }
        }

        files
    }
}

/// Default ignore patterns for code projects
/// These are applied when .gitignore is not available or as fallback
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    // Version control
    ".git",
    ".svn",
    ".hg",
    ".gitmodules",
    // Dependencies
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".pnp",
    ".yarn",
    // Build outputs
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".output",
    "coverage",
    // Gradle/Maven
    ".gradle",
    ".m2",
    "gradle-wrapper.jar",
    // IDE/Editor
    ".idea",
    ".vscode",
    ".fleet",
    "*.swp",
    "*.swo",
    // Cache directories
    ".cache",
    ".parcel-cache",
    ".turbo",
    ".eslintcache",
    ".prettiercache",
    // Generated code
    "generated",
    "gen",
    ".generated",
    // Test artifacts
    ".pytest_cache",
    ".tox",
    "htmlcov",
    // Logs
    "logs",
    "*.log",
    // Temporary
    "tmp",
    "temp",
    // Symora
    ".symora",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_gitignore_integration() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create .gitignore
        fs::write(root.join(".gitignore"), "*.log\ntarget/\n").unwrap();

        // Create test files
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("debug.log"), "log content").unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/app"), "binary").unwrap();

        let filter = FileFilter::with_gitignore(root);

        assert!(filter.should_include(&root.join("main.rs")));
        assert!(!filter.should_include(&root.join("debug.log")));
        assert!(!filter.should_include(&root.join("target")));
        assert!(!filter.should_include(&root.join("target/app")));
    }

    #[test]
    fn test_symora_ignore() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create .symora/ignore
        fs::create_dir(root.join(".symora")).unwrap();
        fs::write(root.join(".symora/ignore"), "*.test.rs\n").unwrap();

        // Create test files
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("main.test.rs"), "test").unwrap();

        let filter = FileFilter::with_gitignore(root);

        assert!(filter.should_include(&root.join("main.rs")));
        assert!(!filter.should_include(&root.join("main.test.rs")));
    }
}
