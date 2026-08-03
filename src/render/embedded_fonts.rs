//! Embedded IBM Plex font data (OTF, Regular+Bold only, DEFLATE-compressed).
//!
//! Hardcoded font bytes eliminate the system font fallback mechanism
//! (fontique FFI backend with 66 unsafe calls) in favor of skrifa/read-fonts
//! (0 unsafe). Supports 11 scripts: Latin, Cyrillic, Greek, Arabic, Hebrew,
//! Devanagari, Thai, Japanese, Simplified Chinese, Traditional Chinese, Korean.
//!
//! Bytes are stored DEFLATE-compressed (via `examples/compress_fonts.rs`,
//! ~20% smaller — OTF/CFF outline data is already fairly dense, so this
//! isn't the 40-60% a text-like asset would see) and inflated once at
//! startup in [`new_font_context`]. `miniz_oxide` + its `adler2` dependency
//! are both `#![forbid(unsafe_code)]` (verified: 0 unsafe blocks in either
//! crate's source), so this adds zero unsafe surface to the font stack.

#![forbid(unsafe_code)]

/// Embedded DEFLATE-compressed OTF font bytes by family and weight.
struct EmbeddedFonts;

impl EmbeddedFonts {
    /// IBM Plex Mono Regular (400)
    const MONO_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexMono-Regular.otf.deflate");

    /// IBM Plex Mono Bold (700)
    const MONO_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexMono-Bold.otf.deflate");

    /// IBM Plex Sans Regular (400)
    const SANS_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSans-Regular.otf.deflate");

    /// IBM Plex Sans Bold (700)
    const SANS_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSans-Bold.otf.deflate");

    /// IBM Plex Serif Regular (400)
    const SERIF_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSerif-Regular.otf.deflate");

    /// IBM Plex Serif Bold (700)
    const SERIF_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSerif-Bold.otf.deflate");

    /// IBM Plex Sans Arabic Regular (400)
    const ARABIC_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansArabic-Regular.otf.deflate");

    /// IBM Plex Sans Arabic Bold (700)
    const ARABIC_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansArabic-Bold.otf.deflate");

    /// IBM Plex Sans Hebrew Regular (400)
    const HEBREW_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansHebrew-Regular.otf.deflate");

    /// IBM Plex Sans Hebrew Bold (700)
    const HEBREW_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansHebrew-Bold.otf.deflate");

    /// IBM Plex Sans Devanagari Regular (400)
    const DEVANAGARI_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansDevanagari-Regular.otf.deflate");

    /// IBM Plex Sans Devanagari Bold (700)
    const DEVANAGARI_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansDevanagari-Bold.otf.deflate");

    /// IBM Plex Sans Thai Regular (400)
    const THAI_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansThai-Regular.otf.deflate");

    /// IBM Plex Sans Thai Bold (700)
    const THAI_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansThai-Bold.otf.deflate");

    /// IBM Plex Sans JP Regular (400)
    const JP_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansJP-Regular.otf.deflate");

    /// IBM Plex Sans JP Bold (700)
    const JP_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansJP-Bold.otf.deflate");

    /// IBM Plex Sans SC (Simplified Chinese) Regular (400)
    const SC_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansSC-Regular.otf.deflate");

    /// IBM Plex Sans SC (Simplified Chinese) Bold (700)
    const SC_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansSC-Bold.otf.deflate");

    /// IBM Plex Sans TC (Traditional Chinese) Regular (400)
    const TC_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansTC-Regular.otf.deflate");

    /// IBM Plex Sans TC (Traditional Chinese) Bold (700)
    const TC_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansTC-Bold.otf.deflate");

    /// IBM Plex Sans KR (Korean) Regular (400)
    const KR_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansKR-Regular.otf.deflate");

    /// IBM Plex Sans KR (Korean) Bold (700)
    const KR_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts_compressed/IBMPlexSansKR-Bold.otf.deflate");
}

/// Inflates a compressed font blob and registers it, returning the
/// [`FamilyId`] fontique assigned it (derived from the font's own `name`
/// table, so Regular+Bold of the same family collapse into one `FamilyId`
/// across two calls).
fn register(
    collection: &mut parley::fontique::Collection,
    compressed: &'static [u8],
) -> parley::fontique::FamilyId {
    let data = miniz_oxide::inflate::decompress_to_vec(compressed)
        .expect("embedded font data is compressed at build time by compress_fonts and verified \
                 to round-trip there; corruption here means the checked-in .deflate is stale/bad");
    let blob = parley::fontique::Blob::new(std::sync::Arc::new(data));
    let registered = collection.register_fonts(blob, None);
    registered
        .first()
        .map(|(family_id, _)| *family_id)
        .expect("embedded font bytes must contain at least one valid font")
}

/// Builds a [`parley::FontContext`] backed *only* by the embedded IBM Plex
/// fonts — no OS font-directory access.
///
/// This matters beyond disk footprint: `parley::fontique::Collection::default()`
/// (what `parley::FontContext::new()` calls) eagerly runs
/// `CollectionOptions::default().system_fonts = true`, which constructs a
/// platform `SystemFonts` backend (DirectWrite on Windows, CoreText on
/// macOS, fontconfig on Linux) — the *only* place unsafe code exists in the
/// parley/fontique/skrifa stack (66 unsafe blocks, all FFI). Merely omitting
/// a later `load_system_fonts()` call does not avoid this: that method is a
/// no-op once `Collection::new` has already installed a `System`. The only
/// way to skip the FFI backend entirely is to construct the `Collection`
/// with `system_fonts: false` from the start, which is what this function
/// does.
///
/// Generic families (`sans-serif`/`serif`/`monospace`) resolve to the
/// bundled Latin/Cyrillic/Greek-covering IBM Plex faces (release notes
/// confirm Cyrillic + monotonic Greek coverage in Sans/Serif/Mono, so no
/// separate script fallback is needed for those three scripts). Script
/// fallback is registered explicitly for the other 8 documented scripts —
/// see `text_engine::tests::script_coverage_bar_renders_without_tofu`.
pub fn new_font_context() -> parley::FontContext {
    let mut collection = parley::fontique::Collection::new(parley::fontique::CollectionOptions {
        shared: false,
        system_fonts: false,
    });

    let mono_regular = register(&mut collection, EmbeddedFonts::MONO_REGULAR);
    register(&mut collection, EmbeddedFonts::MONO_BOLD);
    collection.set_generic_families(
        parley::fontique::GenericFamily::Monospace,
        [mono_regular].into_iter(),
    );

    let sans_regular = register(&mut collection, EmbeddedFonts::SANS_REGULAR);
    register(&mut collection, EmbeddedFonts::SANS_BOLD);
    collection.set_generic_families(
        parley::fontique::GenericFamily::SansSerif,
        [sans_regular].into_iter(),
    );

    let serif_regular = register(&mut collection, EmbeddedFonts::SERIF_REGULAR);
    register(&mut collection, EmbeddedFonts::SERIF_BOLD);
    collection.set_generic_families(
        parley::fontique::GenericFamily::Serif,
        [serif_regular].into_iter(),
    );

    // Script-based fallback (used by parley's per-run shaping regardless of
    // the requested generic family — see text_engine::mod's determinism
    // note). `FallbackKey::from(Script)` with no locale hits fontique's
    // "default" bucket for that script, which is what a run gets when the
    // ancestor `lang` attribute is absent *or* set to that script's primary
    // language (fontique's own canonical-locale table maps e.g. lang="ar"
    // to the same default bucket as no lang at all).
    let arabic_regular = register(&mut collection, EmbeddedFonts::ARABIC_REGULAR);
    register(&mut collection, EmbeddedFonts::ARABIC_BOLD);
    collection.set_fallbacks(
        parley::fontique::Script::from_bytes(*b"Arab"),
        [arabic_regular].into_iter(),
    );

    let hebrew_regular = register(&mut collection, EmbeddedFonts::HEBREW_REGULAR);
    register(&mut collection, EmbeddedFonts::HEBREW_BOLD);
    collection.set_fallbacks(
        parley::fontique::Script::from_bytes(*b"Hebr"),
        [hebrew_regular].into_iter(),
    );

    let devanagari_regular = register(&mut collection, EmbeddedFonts::DEVANAGARI_REGULAR);
    register(&mut collection, EmbeddedFonts::DEVANAGARI_BOLD);
    collection.set_fallbacks(
        parley::fontique::Script::from_bytes(*b"Deva"),
        [devanagari_regular].into_iter(),
    );

    let thai_regular = register(&mut collection, EmbeddedFonts::THAI_REGULAR);
    register(&mut collection, EmbeddedFonts::THAI_BOLD);
    collection.set_fallbacks(
        parley::fontique::Script::from_bytes(*b"Thai"),
        [thai_regular].into_iter(),
    );

    // Han unification: one script, four disjoint glyph sets. fontique's own
    // canonical-locale table (fallback.rs) routes lang="zh" (no region) to
    // the "default" bucket, lang="ja"/"ko" and lang="zh" with a TW/HK/MO
    // region to separate locale-keyed buckets — so each must be registered
    // against the exact `(Script, &str)` pair fontique canonicalizes to.
    let jp_regular = register(&mut collection, EmbeddedFonts::JP_REGULAR);
    register(&mut collection, EmbeddedFonts::JP_BOLD);
    let sc_regular = register(&mut collection, EmbeddedFonts::SC_REGULAR);
    register(&mut collection, EmbeddedFonts::SC_BOLD);
    let tc_regular = register(&mut collection, EmbeddedFonts::TC_REGULAR);
    register(&mut collection, EmbeddedFonts::TC_BOLD);
    let kr_regular = register(&mut collection, EmbeddedFonts::KR_REGULAR);
    register(&mut collection, EmbeddedFonts::KR_BOLD);

    let han = parley::fontique::Script::from_bytes(*b"Hani");
    // No lang, or lang="zh" with no region: IBM's own fallback table already
    // documents "default to simplified Chinese" for this bucket.
    collection.set_fallbacks(han, [sc_regular].into_iter());
    collection.set_fallbacks((han, "ja"), [jp_regular].into_iter());
    collection.set_fallbacks((han, "ko"), [kr_regular].into_iter());
    collection.set_fallbacks((han, "zh-TW"), [tc_regular].into_iter());
    collection.set_fallbacks((han, "zh-HK"), [tc_regular].into_iter());
    collection.set_fallbacks((han, "zh-MO"), [tc_regular].into_iter());

    // Korean text is written in Hangul syllables — script "Hang", a
    // *different* ISO 15924 code from "Hani" (Han/CJK ideographs). Han
    // unification (the fallback map above) only covers Korean text that
    // uses Chinese-derived Hanja; plain Hangul needs its own script
    // fallback entry, or every Korean run shapes to tofu regardless of the
    // `lang="ko"` Hani bucket above (caught by manually running the app —
    // see script_coverage_bar_renders_without_tofu, which exercises real
    // Hangul text and would have failed silently otherwise).
    collection.set_fallbacks(
        parley::fontique::Script::from_bytes(*b"Hang"),
        [kr_regular].into_iter(),
    );

    parley::FontContext {
        collection,
        source_cache: parley::fontique::SourceCache::default(),
    }
}
