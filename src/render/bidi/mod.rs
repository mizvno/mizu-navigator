//! Bidirectional text direction resolution and URL-bar anti-spoofing
//! sanitization (ux-7). See `docs/design/bidi.md` for the full design memo
//! this module implements.
//!
//! ## What this module does *not* do
//!
//! It does not reorder glyphs or implement the Unicode Bidi Algorithm —
//! `parley::bidi::BidiResolver` already does that internally for every
//! layout, unconditionally (verified against parley 0.10's source; see the
//! design memo). This module only resolves *which base direction* a node
//! should use (walking `dir` attribute inheritance) and provides the one
//! lever parley's public API exposes for influencing that: prepending a
//! zero-width strong-directional mark to the text handed to the builder.
//!
//! ## Security posture
//!
//! Pure text-shaping/layout-mirroring — no capability, no I/O, no taint.
//! The one exception is [`strip_bidi_overrides`], which exists specifically
//! to neutralize the classic bidi-spoofing surface (RTL-override control
//! characters disguising a URL) in the one place a user makes a trust
//! decision based on rendered text — the chrome URL bar. Document body text
//! is deliberately left untouched (see the design memo §4): isolates
//! (U+2066–U+2069) are legitimate and necessary for correctly authoring
//! mixed-direction content.

#![forbid(unsafe_code)]

use ego_tree::NodeRef;

use crate::parser::MizuNode;

/// A node's resolved text/base direction, per `dir` attribute inheritance.
///
/// `Auto` means no ancestor (including the node itself) declared an
/// explicit `ltr`/`rtl` — i.e. `dir="auto"` (the default) all the way to
/// the root. Layout consumers (flex mirroring, logical-property
/// resolution) treat `Auto` as `Ltr` (Taffy has no auto-detection
/// concept); text-shaping consumers treat it as "let parley auto-detect
/// from this run's own characters" — see [`Self::prepend_mark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedDirection {
    /// Explicit `dir="ltr"` on this node or the nearest ancestor that set one.
    Ltr,
    /// Explicit `dir="rtl"` on this node or the nearest ancestor that set one.
    Rtl,
    /// No explicit `ltr`/`rtl` found; `dir="auto"` (or unset) throughout.
    Auto,
}

impl ResolvedDirection {
    /// Whether layout (flex-container mirroring, logical-property
    /// resolution) should treat this as right-to-left. `Auto` resolves to
    /// `false` (LTR) for layout purposes — Taffy has no text-content-based
    /// auto-detection to defer to, unlike parley for text shaping.
    pub fn is_rtl_for_layout(self) -> bool {
        matches!(self, Self::Rtl)
    }

    /// The zero-width strong-directional mark to prepend to text handed to
    /// parley's layout builder, so its internal auto-detection (which
    /// always runs — see the module doc) resolves to the explicit
    /// direction instead of whatever the text's own first strong character
    /// would otherwise imply. `None` for `Auto`: nothing to prepend: let
    /// parley's native auto-detection run unmodified.
    pub fn prepend_mark(self) -> Option<char> {
        match self {
            Self::Ltr => Some('\u{200E}'), // LRM
            Self::Rtl => Some('\u{200F}'), // RLM
            Self::Auto => None,
        }
    }
}

/// Resolves `node`'s direction by walking `dir` attribute inheritance:
/// checks `node` itself, then each ancestor in turn, for an explicit
/// `dir="ltr"` or `dir="rtl"` (an explicit `dir="auto"` does not stop the
/// walk — it means the same as not having the attribute at all). Returns
/// [`ResolvedDirection::Auto`] if none is found all the way to the root.
///
/// `O(tree depth)`, not `O(document size)` — called per node, same cost
/// class as any other per-node ancestor walk already in this codebase
/// (e.g. `render::window::focus`'s click-event ancestor search).
pub fn resolve_direction(node: NodeRef<'_, MizuNode>) -> ResolvedDirection {
    let mut current = Some(node);
    while let Some(n) = current {
        match n.value().attributes.get("dir").map(String::as_str) {
            Some("ltr") => return ResolvedDirection::Ltr,
            Some("rtl") => return ResolvedDirection::Rtl,
            _ => {}
        }
        current = n.parent();
    }
    ResolvedDirection::Auto
}

/// Resolves `node`'s language by walking `lang` attribute inheritance:
/// checks `node` itself, then each ancestor in turn, for an explicit `lang`
/// value (shape-validated at parse time by `parser::layout::is_valid_lang_tag`
/// — never re-validated here). Returns `None` if no ancestor (including the
/// document root) set one.
///
/// Mirrors [`resolve_direction`]'s ancestor walk exactly; the only
/// difference is the return type, since `lang` has no closed set of values
/// to collapse onto (unlike `dir`'s `Ltr`/`Rtl`/`Auto`).
pub fn resolve_lang(node: NodeRef<'_, MizuNode>) -> Option<String> {
    let mut current = Some(node);
    while let Some(n) = current {
        if let Some(lang) = n.value().attributes.get("lang") {
            return Some(lang.clone());
        }
        current = n.parent();
    }
    None
}

/// Unicode bidi embedding/override controls (U+202A–U+202E) and isolates
/// (U+2066–U+2069) — see the design memo §4 for why these two ranges
/// specifically, and why they're stripped here but not from document text.
fn is_bidi_override_or_isolate(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Strips bidi embedding/override/isolate control characters from `s`.
///
/// Deletes rather than replaces with a placeholder: a deleted character
/// cannot be used to reconstruct a different-looking string, whereas a
/// visible placeholder glyph would still occupy a position an attacker
/// could design around. Applied at every point the chrome URL bar's text
/// can be written (typed input, paste, and programmatic assignment after a
/// navigation) — never to document body text, which legitimately needs
/// isolates for correct multilingual authoring.
pub fn strip_bidi_overrides(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(is_bidi_override_or_isolate) {
        std::borrow::Cow::Owned(
            s.chars()
                .filter(|c| !is_bidi_override_or_isolate(*c))
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests;
