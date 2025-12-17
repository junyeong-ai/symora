//! Project service for Symora

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::ProjectError;
use crate::models::symbol::Language;

/// Project service trait
#[async_trait]
pub trait ProjectService: Send + Sync {
    /// Initialize a new project
    async fn init(&self, name: Option<&str>, force: bool) -> Result<ProjectInfo, ProjectError>;

    /// Get project status
    async fn status(&self) -> Result<ProjectStatus, ProjectError>;

    /// Check if project is initialized
    fn is_initialized(&self) -> bool;

    /// Detect languages in project
    fn detect_languages(&self) -> Vec<Language>;
}

/// Project information
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// Project name
    pub name: String,
    /// Project root path
    pub root: PathBuf,
    /// Detected languages
    pub languages: Vec<Language>,
    /// Config file path
    pub config_path: PathBuf,
}

/// Project status
#[derive(Debug, Clone)]
pub struct ProjectStatus {
    /// Whether project is initialized
    pub initialized: bool,
    /// Project info if initialized
    pub project: Option<ProjectInfo>,
    /// LSP server status by language
    pub lsp_status: Vec<(Language, String)>,
}

/// Default project service
pub struct DefaultProjectService {
    root: PathBuf,
}

impl DefaultProjectService {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn symora_dir(&self) -> PathBuf {
        self.root.join(".symora")
    }

    fn config_path(&self) -> PathBuf {
        self.symora_dir().join("config.toml")
    }
}

#[async_trait]
impl ProjectService for DefaultProjectService {
    async fn init(&self, name: Option<&str>, force: bool) -> Result<ProjectInfo, ProjectError> {
        let symora_dir = self.symora_dir();

        if symora_dir.exists() && !force {
            return Err(ProjectError::AlreadyExists(self.root.clone()));
        }

        // Create .symora directory
        tokio::fs::create_dir_all(&symora_dir).await?;

        // Detect languages
        let languages = self.detect_languages();

        // Determine project name
        let project_name = name
            .map(|n| n.to_string())
            .or_else(|| {
                self.root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "unnamed".to_string());

        // Create config
        let config = crate::models::config::SymoraConfig {
            project: crate::models::config::ProjectConfig {
                name: Some(project_name.clone()),
                languages: languages.clone(),
                ..Default::default()
            },
            ..Default::default()
        };

        let content = toml::to_string_pretty(&config)
            .map_err(|e| ProjectError::Io(std::io::Error::other(e)))?;
        tokio::fs::write(self.config_path(), content).await?;

        Ok(ProjectInfo {
            name: project_name,
            root: self.root.clone(),
            languages,
            config_path: self.config_path(),
        })
    }

    async fn status(&self) -> Result<ProjectStatus, ProjectError> {
        if !self.is_initialized() {
            return Ok(ProjectStatus {
                initialized: false,
                project: None,
                lsp_status: vec![],
            });
        }

        // Load config
        let config_content = tokio::fs::read_to_string(self.config_path()).await?;
        let config: crate::models::config::SymoraConfig = toml::from_str(&config_content)
            .map_err(|e| ProjectError::Io(std::io::Error::other(e)))?;

        let name = config
            .project
            .name
            .or_else(|| {
                self.root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "unnamed".to_string());

        let languages = if config.project.languages.is_empty() {
            self.detect_languages()
        } else {
            config.project.languages
        };

        Ok(ProjectStatus {
            initialized: true,
            project: Some(ProjectInfo {
                name,
                root: self.root.clone(),
                languages,
                config_path: self.config_path(),
            }),
            lsp_status: vec![], // Will be populated by LSP service
        })
    }

    fn is_initialized(&self) -> bool {
        self.config_path().exists()
    }

    fn detect_languages(&self) -> Vec<Language> {
        let mut languages = HashSet::new();

        // Walk directory and detect languages from file extensions
        let walker = walkdir::WalkDir::new(&self.root)
            .max_depth(5)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.')
                    && !matches!(
                        name.as_ref(),
                        "node_modules" | "target" | "build" | "dist" | "__pycache__" | "venv"
                    )
            });

        for entry in walker.filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let lang = Language::from_path(entry.path());
                if lang != Language::Unknown {
                    languages.insert(lang);
                }
            }
        }

        languages.into_iter().collect()
    }
}
