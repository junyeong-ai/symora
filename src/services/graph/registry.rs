use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, UNIX_EPOCH};

use super::types::SymbolId;

pub struct FileRegistry {
    path_to_id: HashMap<PathBuf, u32>,
    id_to_entry: HashMap<u32, FileEntry>,
    next_id: u32,
}

pub struct FileEntry {
    pub path: PathBuf,
    pub file_mtime: u64,
    pub symbols: Vec<SymbolId>,
    pub last_accessed: Instant,
}

pub fn get_file_mtime(path: &PathBuf) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl FileRegistry {
    pub fn new() -> Self {
        Self {
            path_to_id: HashMap::new(),
            id_to_entry: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn get_or_create_id(&mut self, path: &PathBuf) -> u32 {
        if let Some(&id) = self.path_to_id.get(path) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.path_to_id.insert(path.clone(), id);
        self.id_to_entry.insert(
            id,
            FileEntry {
                path: path.clone(),
                file_mtime: get_file_mtime(path),
                symbols: Vec::new(),
                last_accessed: Instant::now(),
            },
        );
        id
    }

    pub fn get_id(&self, path: &PathBuf) -> Option<u32> {
        self.path_to_id.get(path).copied()
    }

    pub fn get_path(&self, id: u32) -> Option<&PathBuf> {
        self.id_to_entry.get(&id).map(|e| &e.path)
    }

    pub fn get_entry(&self, id: u32) -> Option<&FileEntry> {
        self.id_to_entry.get(&id)
    }

    pub fn get_entry_mut(&mut self, id: u32) -> Option<&mut FileEntry> {
        self.id_to_entry.get_mut(&id)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&u32, &FileEntry)> {
        self.id_to_entry.iter()
    }

    pub fn clear(&mut self) {
        self.path_to_id.clear();
        self.id_to_entry.clear();
        self.next_id = 0;
    }

    pub fn file_count(&self) -> usize {
        self.id_to_entry.len()
    }
}

impl Default for FileRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_create_and_get() {
        let mut registry = FileRegistry::new();
        let path = PathBuf::from("/test/file.rs");

        let id1 = registry.get_or_create_id(&path);
        let id2 = registry.get_or_create_id(&path);

        assert_eq!(id1, id2);
        assert_eq!(registry.get_id(&path), Some(id1));
    }

    #[test]
    fn test_registry_multiple_files() {
        let mut registry = FileRegistry::new();
        let path1 = PathBuf::from("/test/file1.rs");
        let path2 = PathBuf::from("/test/file2.rs");

        let id1 = registry.get_or_create_id(&path1);
        let id2 = registry.get_or_create_id(&path2);

        assert_ne!(id1, id2);
        assert_eq!(registry.file_count(), 2);
    }
}
