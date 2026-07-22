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
        std::borrow::Cow::Owned(s.chars().filter(|c| !is_bidi_override_or_isolate(*c)).collect())
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(dir: Option<&str>) -> MizuNode {
        let mut attributes = HashMap::new();
        if let Some(d) = dir {
            attributes.insert("dir".to_string(), d.to_string());
        }
        MizuNode {
            primitive: crate::parser::Primitive::Box,
            attributes,
            events: HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        }
    }

    #[test]
    fn resolve_direction_finds_explicit_dir_on_self() {
        let tree = ego_tree::Tree::new(node(Some("rtl")));
        assert_eq!(resolve_direction(tree.root()), ResolvedDirection::Rtl);
    }

    #[test]
    fn resolve_direction_inherits_from_ancestor() {
        let mut tree = ego_tree::Tree::new(node(Some("rtl")));
        let child_id = tree.root_mut().append(node(None)).id();
        let grandchild_id = tree.get_mut(child_id).unwrap().append(node(None)).id();
        assert_eq!(
            resolve_direction(tree.get(grandchild_id).unwrap()),
            ResolvedDirection::Rtl,
            "an unset dir must inherit from the nearest ancestor that set one"
        );
    }

    #[test]
    fn resolve_direction_explicit_auto_does_not_stop_inheritance() {
        let mut tree = ego_tree::Tree::new(node(Some("rtl")));
        let child_id = tree.root_mut().append(node(Some("auto"))).id();
        assert_eq!(
            resolve_direction(tree.get(child_id).unwrap()),
            ResolvedDirection::Rtl,
            "`dir=\"auto\"` must not shadow an ancestor's explicit direction"
        );
    }

    #[test]
    fn resolve_direction_child_overrides_ancestor() {
        let mut tree = ego_tree::Tree::new(node(Some("rtl")));
        let child_id = tree.root_mut().append(node(Some("ltr"))).id();
        assert_eq!(resolve_direction(tree.get(child_id).unwrap()), ResolvedDirection::Ltr);
    }

    #[test]
    fn resolve_direction_defaults_to_auto_with_no_dir_anywhere() {
        let tree = ego_tree::Tree::new(node(None));
        assert_eq!(resolve_direction(tree.root()), ResolvedDirection::Auto);
    }

    #[test]
    fn is_rtl_for_layout_treats_auto_as_ltr() {
        assert!(!ResolvedDirection::Auto.is_rtl_for_layout());
        assert!(!ResolvedDirection::Ltr.is_rtl_for_layout());
        assert!(ResolvedDirection::Rtl.is_rtl_for_layout());
    }

    #[test]
    fn prepend_mark_matches_explicit_direction_only() {
        assert_eq!(ResolvedDirection::Ltr.prepend_mark(), Some('\u{200E}'));
        assert_eq!(ResolvedDirection::Rtl.prepend_mark(), Some('\u{200F}'));
        assert_eq!(ResolvedDirection::Auto.prepend_mark(), None);
    }

    #[test]
    fn strip_bidi_overrides_removes_rlo_and_isolates() {
        let input = "evil\u{202E}gnp.exe\u{2066}safe\u{2069}";
        let stripped = strip_bidi_overrides(input);
        assert_eq!(stripped, "evilgnp.exesafe");
        assert!(!stripped.chars().any(is_bidi_override_or_isolate));
    }

    #[test]
    fn strip_bidi_overrides_leaves_clean_strings_untouched() {
        let input = "mizu://example.com/page";
        let stripped = strip_bidi_overrides(input);
        assert_eq!(stripped, input);
        assert!(matches!(stripped, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn strip_bidi_overrides_does_not_touch_legitimate_bidi_text() {
        // Sanity: only the specific override/isolate ranges are stripped —
        // ordinary Hebrew/Arabic text (which is NOT in either stripped
        // range; it's just letters with strong bidi *properties*, not
        // format control characters) must pass through untouched.
        let hebrew = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}"; // "שלום"
        let stripped = strip_bidi_overrides(hebrew);
        assert_eq!(stripped, hebrew);
    }
}
