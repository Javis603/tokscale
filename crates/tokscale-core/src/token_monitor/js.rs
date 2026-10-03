//! JavaScript-compatible path identities and numeric limits for fork-owned clients.

use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// `Number.MAX_SAFE_INTEGER`. A token count above it cannot be represented by
/// the JavaScript adapters either, and summing it would overflow `i64`.
pub(super) const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `path.normalize`: drops `.` components and repeated separators and folds
/// `..` lexically, without touching the filesystem.
pub(super) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let parent_is_normal =
                    matches!(out.components().next_back(), Some(Component::Normal(_)));
                if parent_is_normal {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `sha256(path.normalize(path)).slice(0, 12)`, the namespace the adapters
/// put into session ids.
pub(super) fn path_namespace(path: &Path) -> String {
    let normalized = normalize_path(path);
    Sha256::digest(normalized.to_string_lossy().as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_matches_node() {
        let cases = [
            ("/a/./b//c/../d", "/a/b/d"),
            ("/a/b/..", "/a"),
            ("/..", "/"),
            ("a/../../b", "../b"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_path(Path::new(input)),
                PathBuf::from(expected),
                "{input}"
            );
        }
        assert_eq!(
            path_namespace(Path::new("/home/u/./.qoder-cn//projects")),
            path_namespace(Path::new("/home/u/.qoder-cn/projects"))
        );
    }
}
