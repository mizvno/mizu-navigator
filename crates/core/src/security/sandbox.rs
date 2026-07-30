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
#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use std::path::PathBuf;

    const SEGMENTS: &[&str] = &["a", "b", "..", "."];

    #[kani::proof]
    #[kani::unwind(6)]
    fn normalize_path_components_never_panics() {
        let mut p = PathBuf::new();
        for _ in 0..4 {
            let take: bool = kani::any();
            if take {
                let idx: usize = kani::any();
                kani::assume(idx < SEGMENTS.len());
                p.push(SEGMENTS[idx]);
            }
        }
        let _ = normalize_path_components(&p);
    }

    #[kani::proof]
    fn normalize_path_components_resolves_dotdot() {
        let normalized = normalize_path_components(std::path::Path::new("a/b/../c"));
        assert_eq!(normalized, std::path::Path::new("a/c"));
    }

    #[kani::proof]
    fn normalize_path_components_escape_above_root_is_empty() {
        let normalized = normalize_path_components(std::path::Path::new("../../etc/passwd"));
        assert!(normalized.as_os_str().is_empty());
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn contained_target_without_traversal_stays_contained() {
        // A target built by only appending "a"/"b" segments onto a base
        // (never ".."/".") can never escape: the pure lexical containment
        // check must always report it contained.
        let base = PathBuf::from("base");
        let mut target = base.clone();
        for _ in 0..3 {
            let idx: usize = kani::any();
            kani::assume(idx < 2); // only "a"/"b", never ".."/"."
            target.push(SEGMENTS[idx]);
        }
        let norm_base = normalize_path_components(&base);
        let norm_target = normalize_path_components(&target);
        assert!(!norm_base.as_os_str().is_empty());
        assert!(!norm_target.as_os_str().is_empty());
        assert!(norm_target.starts_with(&norm_base));
    }
}
