//! Outputs derived from raw LSP responses (definition, hover, diagnostics,
//! call/type hierarchies, signature help).

use std::path::Path;

use serde::Serialize;

use super::{LocationOutput, PositionOutput};
use crate::models::lsp::IndexingDegradation;

/// An answer whose completeness depends on workspace indexing. A scalar
/// "nothing here" and a settled "nothing here" are the same JSON without
/// this, so the helper that produces one attaches the state it ran under.
pub trait DisclosesIndexing {
    fn with_indexing(self, indexing: Option<IndexingDegradation>) -> Self;
}

#[derive(Debug, Serialize)]
pub struct DefinitionOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingDegradation>,
    /// A definition in another checkout's copy of a workspace member the
    /// Python server resolves elsewhere (`member_shadowed`) or is not known
    /// to resolve here (`member_unresolved`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub coverage_gaps: Vec<super::CoverageGap>,
    /// Which of this tree's files a definition outside the project is
    /// another checkout's copy of, where the relative path proves it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HoverOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The range the server says the hover covers, where it names one.
    #[serde(flatten)]
    pub range: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<IndexingDegradation>,
}

/// The message for a position the server named nothing at. Where its
/// readiness wait ran out the server may not have read the answer yet, so
/// the message says the answer is not known rather than absent — what
/// `typedef` and `implementations` carry in that state as an empty list
/// under the same `indexing` marker. `unconfirmed` keeps the plain wording:
/// most servers never confirm readiness, and the marker already qualifies it.
fn not_found_message(absent: &str, unknown: &str, indexing: Option<IndexingDegradation>) -> String {
    match indexing {
        Some(IndexingDegradation::TimedOut) => format!(
            "{unknown}: the server's readiness wait ran out before it reported the workspace \
             loaded, so this is not an absence; retry once it is warm"
        ),
        _ => absent.to_string(),
    }
}

impl DefinitionOutput {
    /// No definition at the position, under the indexing state it was asked in.
    pub fn not_found(indexing: Option<IndexingDegradation>) -> Self {
        Self {
            definition: None,
            message: Some(not_found_message(
                "No definition found",
                "No definition known yet",
                indexing,
            )),
            indexing,
            coverage_gaps: Vec::new(),
            hints: Vec::new(),
        }
    }
}

impl HoverOutput {
    /// No hover information at the position, under the indexing state it
    /// was asked in.
    pub fn not_found(indexing: Option<IndexingDegradation>) -> Self {
        Self {
            content: None,
            range: None,
            message: Some(not_found_message(
                "No hover information available",
                "No hover information known yet",
                indexing,
            )),
            indexing,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DiagnosticOutput {
    pub severity: String,
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<&crate::models::diagnostic::Diagnostic> for DiagnosticOutput {
    fn from(d: &crate::models::diagnostic::Diagnostic) -> Self {
        Self {
            severity: d.severity.to_string(),
            message: d.message.clone(),
            line: d.display_line(),
            column: d.display_column(),
            end_line: d.display_end_line(),
            end_column: d.display_end_column(),
            code: d.code.clone(),
            source: d.source.clone(),
            tags: d.tags.iter().map(|t| t.to_string()).collect(),
        }
    }
}

/// Where a call is made. A caller holds the call, so the caller's own
/// position names the file and the site states its line and column; a
/// callee is called from code its own position does not name, so the site
/// states the file too.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CallSite {
    Within(PositionOutput),
    At(LocationOutput),
}

#[derive(Debug, Serialize)]
pub struct CallHierarchyOutput {
    pub name: String,
    pub kind: String,
    #[serde(flatten)]
    pub location: LocationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_site: Option<CallSite>,
    /// Complete verbatim source body, set only by `context --with-bodies`
    /// callee attachment — absent everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_omitted: Option<super::BodyOmission>,
}

impl CallHierarchyOutput {
    /// A caller, at its own declaration, with the call it makes there.
    pub fn caller(item: &crate::models::lsp::CallHierarchyItem, root: &Path) -> Self {
        Self::with_site(item, root, |site| {
            CallSite::Within(PositionOutput::from_location(site))
        })
    }

    /// A callee, at its own declaration, with the call made to it in the
    /// code that calls it.
    pub fn callee(item: &crate::models::lsp::CallHierarchyItem, root: &Path) -> Self {
        Self::with_site(item, root, |site| {
            CallSite::At(LocationOutput::from_location(site, root))
        })
    }

    fn with_site(
        item: &crate::models::lsp::CallHierarchyItem,
        root: &Path,
        site: impl FnOnce(&crate::models::symbol::Location) -> CallSite,
    ) -> Self {
        Self {
            name: item.name.clone(),
            kind: item.kind.to_string(),
            location: LocationOutput::from_location(&item.location, root),
            call_site: item.call_site.as_ref().map(site),
            body: None,
            body_omitted: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeInfoOutput {
    pub name: String,
    pub kind: String,
    #[serde(flatten)]
    pub location: LocationOutput,
    /// Complete verbatim source body, set only by `context --with-bodies`
    /// type attachment — absent everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_omitted: Option<super::BodyOmission>,
}

macro_rules! discloses_indexing {
    ($($type:ty),+ $(,)?) => {
        $(impl DisclosesIndexing for $type {
            fn with_indexing(mut self, indexing: Option<IndexingDegradation>) -> Self {
                self.indexing = indexing;
                self
            }
        })+
    };
}

discloses_indexing!(DefinitionOutput, HoverOutput);
