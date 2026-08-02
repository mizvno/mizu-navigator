//! Platform data directory resolution for the persisted history file.

// ── Platform data directory ────────────────────────────────────────────────────

/// Returns the platform-specific application data directory for Mizu.
///
/// | Platform | Path                                       |
/// |----------|--------------------------------------------|
/// | Windows  | `%APPDATA%\mizu`                           |
/// | macOS    | `~/Library/Application Support/mizu`       |
/// | Linux    | `$XDG_DATA_HOME/mizu` or `~/.local/share/mizu` |
pub(super) fn data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(base).join("mizu")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("mizu")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{home}/.local/share")
        });
        std::path::PathBuf::from(base).join("mizu")
    }
}
