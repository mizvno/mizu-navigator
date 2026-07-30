//! File sandbox containment logic.

/// Normalises a path lexically (no I/O) by resolving `.` and `..` components.
///
/// Returns an empty [`std::path::PathBuf`] if the path would escape above its
/// root, ensuring that the `starts_with` sandbox check always fails for
/// path-traversal attempts.
pub fn normalize_path_components(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // pop() returns false when there is nothing left to pop
                // (empty PathBuf or root-only).  In that case the traversal
                // would escape above root — signal failure with an empty path.
                if !out.pop() {
                    return std::path::PathBuf::new();
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Strips Windows' verbatim prefix (`\\?\`) from a path.
///
/// [`std::fs::canonicalize`] on Windows returns verbatim paths whose prefix
/// component (`VerbatimDisk`) never matches the plain `Disk` prefix of a
/// lexically-normalised path, so `Path::starts_with` would always fail when
/// one side was canonicalised and the other was not (e.g. an existing sandbox
/// base vs. a not-yet-existing target).  No-op on non-Windows paths.
fn strip_verbatim_prefix(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest.to_string());
    }
    p
}

/// Returns `true` if `target` is contained within `sandbox_base`.
///
/// Uses [`std::fs::canonicalize`] when both paths exist (resolves symlinks);
/// falls back to [`normalize_path_components`] for non-existent targets (e.g.
/// first-time navigation, unit tests).  Returns `false` when either canonical
/// path is empty (escape detected) or when the target does not start with
/// `sandbox_base`.
pub fn file_sandbox_contains(sandbox_base: &std::path::Path, target: &std::path::Path) -> bool {
    let canon_base = strip_verbatim_prefix(
        std::fs::canonicalize(sandbox_base)
            .unwrap_or_else(|_| normalize_path_components(sandbox_base)),
    );
    let canon_target = strip_verbatim_prefix(
        std::fs::canonicalize(target).unwrap_or_else(|_| normalize_path_components(target)),
    );
    !canon_base.as_os_str().is_empty()
        && !canon_target.as_os_str().is_empty()
        && canon_target.starts_with(&canon_base)
}

// Kani harnesses for the pure sandbox-containment core — see
// `SECURITY-INVARIANTS.md` §8. `file_sandbox_contains` itself calls
// `std::fs::canonicalize` (a real syscall, unsupported/foreign under Kani),
// so these harnesses target `normalize_path_components` plus the
// `starts_with` containment check directly — exactly the fallback branch
// `file_sandbox_contains` uses for not-yet-existing paths, and the part of
// the logic that actually decides traversal safety (canonicalize just
// resolves symlinks first, when the path exists).
//
// Each harness checks exactly one concrete path, which is a deliberate
// limitation and the only shape that terminates here. Two earlier versions did
// not finish inside 25 minutes: one pushed `SEGMENTS[idx]` for a symbolic
// `idx`, handing CBMC a `&str` with a symbolic *pointer* so that
// `Path::components` compared across an unresolved set of allocations; the
// replacement iterated a concrete table of paths instead, which removed the
// symbolic pointer but still multiplied `parse_next_component`'s inner byte
// scan across every entry. One short literal per harness keeps each proof in
// the seconds range. What is bounded is the set of path spellings.
#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use std::path::{Path, PathBuf};

    #[kani::proof]
    #[kani::unwind(12)]
    fn normalize_path_components_never_panics() {
        let _ = normalize_path_components(Path::new("a/./b/../c"));
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn normalize_path_components_resolves_dotdot() {
        assert_eq!(
            normalize_path_components(Path::new("a/b/../c")),
            Path::new("a/c")
        );
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn normalize_path_components_drops_curdir() {
        assert_eq!(normalize_path_components(Path::new("a/./b")), Path::new("a/b"));
    }

    /// A `..` that climbs above the root is rejected outright rather than
    /// silently clamped — the empty path is the failure signal
    /// `file_sandbox_contains` relies on.
    #[kani::proof]
    #[kani::unwind(12)]
    fn normalize_path_components_escape_above_root_is_empty() {
        assert!(
            normalize_path_components(Path::new("../etc"))
                .as_os_str()
                .is_empty()
        );
    }

    /// A target built by appending ordinary segments onto a base can never
    /// escape: the lexical containment check must report it contained.
    #[kani::proof]
    #[kani::unwind(12)]
    fn contained_target_without_traversal_stays_contained() {
        let norm_base = normalize_path_components(&PathBuf::from("base"));
        let norm_target = normalize_path_components(&PathBuf::from("base/a/b"));

        assert!(!norm_base.as_os_str().is_empty());
        assert!(!norm_target.as_os_str().is_empty());
        assert!(norm_target.starts_with(&norm_base));
    }

    /// The mirror image, without which a containment check that always
    /// returned `true` would satisfy the harness above.
    #[kani::proof]
    #[kani::unwind(12)]
    fn escaping_target_is_not_contained() {
        let norm_base = normalize_path_components(&PathBuf::from("base"));
        let norm_target = normalize_path_components(&PathBuf::from("base/../evil"));

        assert!(
            norm_target.as_os_str().is_empty() || !norm_target.starts_with(&norm_base),
            "a target that climbs out of the base must not be contained"
        );
    }
}
