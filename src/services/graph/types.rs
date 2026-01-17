use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::models::symbol::{Location, SymbolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(u64);

impl SymbolId {
    pub fn new(file_id: u32, line: u32, column: u32) -> Self {
        let packed =
            ((file_id as u64) << 32) | ((line as u64 & 0xFF_FFFF) << 8) | (column as u64 & 0xFF);
        Self(packed)
    }

    pub fn file_id(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub fn line(self) -> u32 {
        ((self.0 >> 8) & 0xFF_FFFF) as u32
    }

    pub fn column(self) -> u32 {
        (self.0 & 0xFF) as u32
    }
}

#[derive(Debug, Clone)]
pub struct SymbolNode {
    pub name: Arc<str>,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Reference,
    Calls,
    CalledBy,
    Definition,
    TypeDefinition,
    Implementation,
    Supertype,
    Subtype,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub target: SymbolId,
    pub target_name: Option<Arc<str>>,
    pub target_kind: Option<SymbolKind>,
    pub call_site: Option<Location>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallInfo {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub call_site: Option<Location>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeHierarchyInfo {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_id_packing() {
        let id = SymbolId::new(123, 456, 78);
        assert_eq!(id.file_id(), 123);
        assert_eq!(id.line(), 456);
        assert_eq!(id.column(), 78);
    }

    #[test]
    fn test_symbol_id_max_values() {
        let id = SymbolId::new(u32::MAX, 0xFF_FFFF, 0xFF);
        assert_eq!(id.file_id(), u32::MAX);
        assert_eq!(id.line(), 0xFF_FFFF);
        assert_eq!(id.column(), 0xFF);
    }
}
