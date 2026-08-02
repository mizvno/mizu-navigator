//! # `urls` — Compile-Time URL Registry Parser
//!
//! This module parses the `urls` macro-block produced by [`super::splitter`]
//! into a [`UrlRegistry`] — a symbol-keyed map of named endpoint aliases.
//!
//! ## Syntax
//!
//! Each non-blank line inside the `urls` block declares one endpoint:
//!
//! ```text
//! api   <alias>  <relative-path>
//! media <alias>  <absolute-mizu-url>
//! ```
//!
//! * `api` endpoints must use a relative path starting with `/`.
//! * `media` endpoints must use an absolute `mizu://` URL.
//!
//! ## Compile-Time Guard
//!
//! Both path/URL formats are validated at parse time.  A malformed entry
//! returns [`MizuError::ParseError`] immediately, preventing a malformed
//! URL from silently reaching the network layer.

#![forbid(unsafe_code)]

use rustc_hash::FxHashMap;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol};

/// Distinguishes REST API endpoints from media (binary asset) endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointKind {
    /// A REST-style API endpoint whose path is relative to the server root.
    /// The `raw_target` is always a `/`-prefixed relative path.
    Api,
    /// A binary-asset media endpoint addressed by an absolute `mizu://` URL.
    Media,
}

/// A single resolved endpoint entry stored in the [`UrlRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlEndpoint {
    /// Whether this is an API or media endpoint.
    pub kind: EndpointKind,
    /// The validated target string:
    /// * For [`EndpointKind::Api`]: a `/`-prefixed relative path.
    /// * For [`EndpointKind::Media`]: an absolute `mizu://` URL.
    pub raw_target: String,
}

/// A symbol-keyed map of all named endpoint aliases declared in the `urls` block.
///
/// [`Symbol`] keys are interned at parse time so that downstream modules can
/// look up endpoints in O(1) without heap allocation.
pub type UrlRegistry = FxHashMap<Symbol, UrlEndpoint>;

/// Parses the `urls` macro-block content into a [`UrlRegistry`].
///
/// `content` is the raw, comment-stripped, blank-padded output of
/// [`super::splitter::split_source`].  Each non-blank line must match one of:
///
/// ```text
/// api   <alias>  <path>       — path must start with `/`
/// media <alias>  <mizu-url>   — URL must start with `mizu://`
/// ```
///
/// Aliases are interned into `interner` so that the same identifier string
/// resolves to the same [`Symbol`] across the `logic` and `urls` blocks.
///
/// # Errors
///
/// | Condition | Error |
/// |---|---|
/// | Unknown keyword (not `api` or `media`) | [`MizuError::ParseError`] |
/// | Missing alias or target fields | [`MizuError::ParseError`] |
/// | `api` target does not start with `/` | [`MizuError::ParseError`] |
/// | `media` target does not start with `mizu://` | [`MizuError::ParseError`] |
/// | Duplicate alias | [`MizuError::ParseError`] |
pub fn parse_urls(content: &str, interner: &mut StringInterner) -> Result<UrlRegistry, MizuError> {
    let mut registry: UrlRegistry = FxHashMap::default();

    for (line_idx, raw_line) in content.lines().enumerate() {
        let line_no = line_idx + 1;

        // Skip blank padding lines produced by the splitter.
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Each entry has exactly 3 whitespace-separated tokens.
        let mut parts = trimmed.splitn(3, char::is_whitespace);
        let keyword = parts.next().unwrap_or("");
        let alias_str = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                MizuError::ParseError(format!(
                    "urls line {line_no}: missing alias after `{keyword}`"
                ))
            })?;
        let target = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                MizuError::ParseError(format!(
                    "urls line {line_no}: missing target after alias `{alias_str}`"
                ))
            })?;

        let kind = match keyword {
            "api" => {
                // ── Compile-time guard: api paths must be relative (start with `/`)
                if !target.starts_with('/') {
                    return Err(MizuError::ParseError(format!(
                        "urls line {line_no}: api endpoint `{alias_str}` target \
                         `{target}` must start with `/` (got a non-relative path)"
                    )));
                }
                EndpointKind::Api
            }
            "media" => {
                // ── Compile-time guard: media URLs must be absolute mizu:// URIs
                if !target.starts_with("mizu://") {
                    return Err(MizuError::ParseError(format!(
                        "urls line {line_no}: media endpoint `{alias_str}` target \
                         `{target}` must start with `mizu://` (got a non-absolute URL)"
                    )));
                }
                EndpointKind::Media
            }
            other => {
                return Err(MizuError::ParseError(format!(
                    "urls line {line_no}: unknown endpoint keyword `{other}`; \
                     expected `api` or `media`"
                )));
            }
        };

        let sym = interner.get_or_intern(alias_str);

        // ── Compile-time guard: duplicate alias
        if registry.contains_key(&sym) {
            return Err(MizuError::ParseError(format!(
                "urls line {line_no}: duplicate alias `{alias_str}` — \
                 each alias must be unique within the `urls` block"
            )));
        }

        registry.insert(
            sym,
            UrlEndpoint {
                kind,
                raw_target: target.to_owned(),
            },
        );
    }

    Ok(registry)
}

// No Kani harness for `parse_urls` here (see `SECURITY-INVARIANTS.md` §8 for
// the rest of this crate's Kani coverage). `parse_urls` interns aliases via
// `StringInterner`, which is backed by a `HashMap`. Even with a fully
// bounded, tiny symbolic `content` string, CBMC's exploration of the
// hashing/allocation paths reached through `HashMap` insertion caused the
// same kind of path explosion documented for `flow.rs`'s taint-propagation
// core — a known limitation of bounded model checking against hash-based
// lookup tables, not something bounding the input further would fix.

#[cfg(test)]
mod tests;
