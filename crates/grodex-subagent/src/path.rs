//! AgentPath — stable, human-readable tree-shaped address for agents.
//!
//! Design Doc 12 §10: AgentPath is unique within a root tree. It provides
//! a human- and model-readable address like `/root/reviewer/security`.
//! Renaming an agent does not change its `AgentId`; the path is a
//! navigation convenience, not an identity.
//!
//! Paths are slash-delimited, start with `/`, and use URL-safe segment
//! names. The root agent has path `/root`. Children append their label:
//! `/root/reviewer`, `/root/reviewer/security`, etc.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A tree-shaped agent address, e.g. `/root/reviewer/security`.
///
/// Invariants:
/// - Always starts with `/`.
/// - Never empty.
/// - Segments are non-empty and contain no `/`.
/// - The first segment is always `root`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentPath(String);

impl AgentPath {
    /// The root agent's path.
    pub const ROOT: &'static str = "/root";

    /// Create the root path.
    pub fn root() -> Self {
        Self(Self::ROOT.to_string())
    }

    /// Create a path from a string. Validates the format.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if !s.starts_with('/') {
            return Err(PathError::MissingLeadingSlash);
        }
        let trimmed = &s[1..];
        if trimmed.is_empty() {
            return Err(PathError::Empty);
        }
        for segment in trimmed.split('/') {
            if segment.is_empty() {
                return Err(PathError::EmptySegment);
            }
        }
        Ok(Self(s.to_string()))
    }

    /// Append a child segment to this path, returning a new path.
    ///
    /// `child` must be non-empty and contain no `/`.
    pub fn child(&self, child: &str) -> Result<Self, PathError> {
        if child.is_empty() {
            return Err(PathError::Empty);
        }
        if child.contains('/') {
            return Err(PathError::SegmentContainsSlash);
        }
        Ok(Self(format!("{}/{}", self.0, child)))
    }

    /// The parent path, or `None` if this is the root.
    pub fn parent(&self) -> Option<Self> {
        if self.0 == Self::ROOT {
            return None;
        }
        let idx = self.0.rfind('/')?;
        Some(Self(self.0[..idx].to_string()))
    }

    /// The last segment (the agent's own name), or `root` for the root.
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("root")
    }

    /// Depth of this node in the tree. Root = 0, root's children = 1, etc.
    pub fn depth(&self) -> usize {
        self.0.matches('/').count().saturating_sub(1)
    }

    /// Whether this path is an ancestor of (or equal to) `other`.
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        other.0.starts_with(&self.0)
    }

    /// Whether this path is a descendant of `other`.
    pub fn is_descendant_of(&self, other: &Self) -> bool {
        self.0.starts_with(&other.0) && self.0 != other.0
    }

    /// The raw string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for AgentPath {
    fn default() -> Self {
        Self::root()
    }
}

/// Errors that can occur when constructing an `AgentPath`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path must start with '/'")]
    MissingLeadingSlash,
    #[error("path contains an empty segment (consecutive slashes)")]
    EmptySegment,
    #[error("path segment contains '/'")]
    SegmentContainsSlash,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_path() {
        let p = AgentPath::root();
        assert_eq!(p.as_str(), "/root");
        assert_eq!(p.depth(), 0);
        assert_eq!(p.name(), "root");
        assert!(p.parent().is_none());
    }

    #[test]
    fn child_path() {
        let root = AgentPath::root();
        let child = root.child("reviewer").unwrap();
        assert_eq!(child.as_str(), "/root/reviewer");
        assert_eq!(child.depth(), 1);
        assert_eq!(child.name(), "reviewer");
        assert_eq!(child.parent().unwrap().as_str(), "/root");
    }

    #[test]
    fn grandchild_path() {
        let root = AgentPath::root();
        let child = root.child("reviewer").unwrap();
        let grandchild = child.child("security").unwrap();
        assert_eq!(grandchild.as_str(), "/root/reviewer/security");
        assert_eq!(grandchild.depth(), 2);
        assert_eq!(grandchild.parent().unwrap().as_str(), "/root/reviewer");
    }

    #[test]
    fn parse_valid() {
        let p = AgentPath::parse("/root/worker").unwrap();
        assert_eq!(p.as_str(), "/root/worker");
        assert_eq!(p.depth(), 1);
    }

    #[test]
    fn parse_rejects_invalid() {
        assert_eq!(AgentPath::parse("").unwrap_err(), PathError::Empty);
        assert_eq!(
            AgentPath::parse("root").unwrap_err(),
            PathError::MissingLeadingSlash
        );
        assert_eq!(
            AgentPath::parse("/root//child").unwrap_err(),
            PathError::EmptySegment
        );
        assert_eq!(
            AgentPath::parse("/").unwrap_err(),
            PathError::Empty
        );
    }

    #[test]
    fn child_rejects_slash_in_name() {
        let root = AgentPath::root();
        assert_eq!(
            root.child("a/b").unwrap_err(),
            PathError::SegmentContainsSlash
        );
        assert_eq!(root.child("").unwrap_err(), PathError::Empty);
    }

    #[test]
    fn ancestor_descendant() {
        let root = AgentPath::root();
        let child = root.child("a").unwrap();
        let grandchild = child.child("b").unwrap();

        assert!(root.is_ancestor_of(&child));
        assert!(root.is_ancestor_of(&grandchild));
        assert!(child.is_ancestor_of(&grandchild));
        assert!(!child.is_ancestor_of(&root));

        assert!(grandchild.is_descendant_of(&root));
        assert!(grandchild.is_descendant_of(&child));
        assert!(!root.is_descendant_of(&child));
    }

    #[test]
    fn depth_of_deep_paths() {
        let p = AgentPath::parse("/root/a/b/c/d").unwrap();
        assert_eq!(p.depth(), 4);
    }

    #[test]
    fn round_trips_through_json() {
        let p = AgentPath::parse("/root/reviewer/security").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("/root/reviewer/security"));
        let back: AgentPath = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
