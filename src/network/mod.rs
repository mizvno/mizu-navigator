/// Messages for UI thread isolation communication
pub mod messages;
/// OpenNIC DNS resolver — forces all domain lookups through OpenNIC servers
pub mod opennic;
/// URI parser for `mizu://`
pub mod uri;
/// Zero-Touch Vault for credentials
pub mod vault;
/// Asynchronous QUIC worker thread
pub mod worker;
pub use crate::parser::logic::NetworkMethod;
pub use messages::{
    NetworkRequest, ReloadPayload, RuntimeAction, StateUpdate, UiEvent, WorkerResponse,
};

use crate::core::errors::MizuError;

/// Command sent from the UI loop to the networking thread.
#[derive(Debug)]
pub enum NetworkCmd {
    /// Perform a network request
    Fetch {
        method: String,
        url: String,
        target_var: String,
        /// `true` when the document that issued this fetch was loaded from a remote
        /// `mizu://` host.  Retained for API symmetry; `file://` is unconditionally
        /// blocked regardless of this value.
        is_remote_origin: bool,
    },
    /// Perform a full navigation request
    Navigate {
        url: String,
    },
    /// Fetch an image and cache it
    FetchImage {
        url: String,
        /// `true` when the triggering document was loaded from a remote `mizu://` host.
        is_remote_origin: bool,
        /// Sandbox base directory for `file://` asset fetches.
        ///
        /// `Some(dir)` — allow reading `file://` URLs only if the resolved path
        /// starts with `dir` (the parent directory of the current document).
        /// `None` — block all `file://` access unconditionally.
        sandbox_base: Option<String>,
    },
    /// Execute a compile-time–validated HTTP/3 request via a URL alias.
    NetworkRequest {
        request: NetworkRequest,
    },
    /// Persist a key/value pair to encrypted local storage.
    ///
    /// The raw (pre-hash) domain string is sent so that the worker can
    /// call [`crate::core::storage::ValidatedDomain::from_raw`] on the
    /// blocking thread pool, keeping both the keyring IPC and the
    /// file-system write off the UI thread.
    StorageStore {
        /// Raw domain string (e.g. `"example.com"` or `"file_/path/doc"`).
        domain: String,
        key: String,
        value: crate::core::types::Value,
    },
}

/// Result sent from the networking thread back to the UI loop.
#[derive(Debug)]
pub enum NetworkResult {
    /// Request succeeded, with the value to update in VariableStore
    Success {
        target_var: String,
        data: crate::core::types::Value,
    },
    /// Navigation succeeded, returning the new source code to parse
    NavigateSuccess {
        url: String,
        source: String,
    },
    /// The server responded with a redirect
    Redirect {
        new_url: String,
    },
    /// Image fetch succeeded, returning decoded image
    FetchImageSuccess {
        url: String,
        image: crate::render::window::AnimatedImage,
    },
    /// Image fetch failed
    FetchImageFailed {
        url: String,
        error: MizuError,
    },
    /// Request failed
    Error(MizuError),
}
