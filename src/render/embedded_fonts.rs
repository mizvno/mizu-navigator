//! Embedded IBM Plex font data (OTF, Regular+Bold only).
//!
//! Hardcoded font bytes eliminate the system font fallback mechanism
//! (fontique FFI backend with 66 unsafe calls) in favor of skrifa/read-fonts
//! (0 unsafe). Supports 11 scripts: Latin, Cyrillic, Greek, Arabic, Hebrew,
//! Devanagari, Thai, Japanese, Simplified Chinese, Traditional Chinese, Korean.

#![forbid(unsafe_code)]

/// Embedded OTF font bytes by family and weight.
pub struct EmbeddedFonts;

impl EmbeddedFonts {
    /// IBM Plex Mono Regular (400)
    pub const MONO_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-mono/fonts/complete/otf/IBMPlexMono-Regular.otf");

    /// IBM Plex Mono Bold (700)
    pub const MONO_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-mono/fonts/complete/otf/IBMPlexMono-Bold.otf");

    /// IBM Plex Sans Regular (400)
    pub const SANS_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans/fonts/complete/otf/IBMPlexSans-Regular.otf");

    /// IBM Plex Sans Bold (700)
    pub const SANS_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans/fonts/complete/otf/IBMPlexSans-Bold.otf");

    /// IBM Plex Serif Regular (400)
    pub const SERIF_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-serif/fonts/complete/otf/IBMPlexSerif-Regular.otf");

    /// IBM Plex Serif Bold (700)
    pub const SERIF_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-serif/fonts/complete/otf/IBMPlexSerif-Bold.otf");

    /// IBM Plex Sans Arabic Regular (400)
    pub const ARABIC_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-arabic/fonts/complete/otf/IBMPlexSansArabic-Regular.otf");

    /// IBM Plex Sans Arabic Bold (700)
    pub const ARABIC_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-arabic/fonts/complete/otf/IBMPlexSansArabic-Bold.otf");

    /// IBM Plex Sans Hebrew Regular (400)
    pub const HEBREW_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-hebrew/fonts/complete/otf/IBMPlexSansHebrew-Regular.otf");

    /// IBM Plex Sans Hebrew Bold (700)
    pub const HEBREW_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-hebrew/fonts/complete/otf/IBMPlexSansHebrew-Bold.otf");

    /// IBM Plex Sans Devanagari Regular (400)
    pub const DEVANAGARI_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-devanagari/fonts/complete/otf/IBMPlexSansDevanagari-Regular.otf");

    /// IBM Plex Sans Devanagari Bold (700)
    pub const DEVANAGARI_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-devanagari/fonts/complete/otf/IBMPlexSansDevanagari-Bold.otf");

    /// IBM Plex Sans Thai Regular (400)
    pub const THAI_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-thai/fonts/complete/otf/IBMPlexSansThai-Regular.otf");

    /// IBM Plex Sans Thai Bold (700)
    pub const THAI_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-thai/fonts/complete/otf/IBMPlexSansThai-Bold.otf");

    /// IBM Plex Sans JP Regular (400)
    pub const JP_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-jp/fonts/complete/otf/unhinted/IBMPlexSansJP-Regular.otf");

    /// IBM Plex Sans JP Bold (700)
    pub const JP_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-jp/fonts/complete/otf/unhinted/IBMPlexSansJP-Bold.otf");

    /// IBM Plex Sans SC (Simplified Chinese) Regular (400)
    pub const SC_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-sc/fonts/complete/otf/unhinted/IBMPlexSansSC-Regular.otf");

    /// IBM Plex Sans SC (Simplified Chinese) Bold (700)
    pub const SC_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-sc/fonts/complete/otf/unhinted/IBMPlexSansSC-Bold.otf");

    /// IBM Plex Sans TC (Traditional Chinese) Regular (400)
    pub const TC_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-tc/fonts/complete/otf/unhinted/IBMPlexSansTC-Regular.otf");

    /// IBM Plex Sans TC (Traditional Chinese) Bold (700)
    pub const TC_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-tc/fonts/complete/otf/unhinted/IBMPlexSansTC-Bold.otf");

    /// IBM Plex Sans KR (Korean) Regular (400)
    pub const KR_REGULAR: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-kr/fonts/complete/otf/IBMPlexSansKR-Regular.otf");

    /// IBM Plex Sans KR (Korean) Bold (700)
    pub const KR_BOLD: &'static [u8] =
        include_bytes!("../../assets/fonts/ibm-plex-sans-kr/fonts/complete/otf/IBMPlexSansKR-Bold.otf");

    /// Return all embedded fonts as byte slices.
    /// Order: Mono, Sans, Serif, Arabic, Hebrew, Devanagari, Thai, JP, SC, TC, KR.
    pub fn all() -> &'static [&'static [u8]] {
        &[
            Self::MONO_REGULAR,
            Self::MONO_BOLD,
            Self::SANS_REGULAR,
            Self::SANS_BOLD,
            Self::SERIF_REGULAR,
            Self::SERIF_BOLD,
            Self::ARABIC_REGULAR,
            Self::ARABIC_BOLD,
            Self::HEBREW_REGULAR,
            Self::HEBREW_BOLD,
            Self::DEVANAGARI_REGULAR,
            Self::DEVANAGARI_BOLD,
            Self::THAI_REGULAR,
            Self::THAI_BOLD,
            Self::JP_REGULAR,
            Self::JP_BOLD,
            Self::SC_REGULAR,
            Self::SC_BOLD,
            Self::TC_REGULAR,
            Self::TC_BOLD,
            Self::KR_REGULAR,
            Self::KR_BOLD,
        ]
    }
}
