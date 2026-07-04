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
        /// Uppercase HTTP method (`"GET"`, `"POST"`, …).
        method: String,
        /// Fully-resolved `mizu://` target URL.
        url: String,
        /// Variable name the response value is bound to.
        target_var: String,
        /// `true` when the document that issued this fetch was loaded from a remote
        /// `mizu://` host.  Retained for API symmetry; `file://` is unconditionally
        /// blocked regardless of this value.
        is_remote_origin: bool,
        /// Optional request payload (POST / PUT / QUERY).  Serialised to JSON by
        /// the network worker and sent as the HTTP/3 request body with
        /// `Content-Type: application/json`.  `None` for body-less methods.
        payload: Option<crate::core::types::Value>,
    },
    /// Perform a full navigation request
    Navigate {
        /// The target document's URL.
        url: String,
    },
    /// Fetch an image and cache it
    FetchImage {
        /// The resolved image URL (`mizu://`, `file://`, …).
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
        /// The alias-resolved request description.
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
        /// The key under which `value` is stored.
        key: String,
        /// The value to persist.
        value: crate::core::types::Value,
    },
}

/// Result sent from the networking thread back to the UI loop.
#[derive(Debug)]
pub enum NetworkResult {
    /// Request succeeded, with the value to update in VariableStore
    Success {
        /// Variable name the response value is bound to.
        target_var: String,
        /// The decoded response value.
        data: crate::core::types::Value,
    },
    /// A `Fetch` request failed.  Carries the bound variable so the UI can
    /// surface a readable error message exactly where the response would have
    /// gone (e.g. `Status: error: connection refused`), instead of failing
    /// silently.
    FetchFailed {
        /// The variable the fetch result was bound to (`GET(alias) -> var`).
        target_var: String,
        /// The failure that aborted the request.
        error: MizuError,
    },
    /// Navigation succeeded, returning the new source code to parse
    NavigateSuccess {
        /// The navigated-to document's URL.
        url: String,
        /// The raw `.mizu` source fetched from that URL.
        source: String,
    },
    /// The server responded with a redirect
    Redirect {
        /// The redirect target URL to navigate to next.
        new_url: String,
    },
    /// Image fetch succeeded, returning decoded image
    FetchImageSuccess {
        /// The URL the image was fetched from (cache key).
        url: String,
        /// The decoded, ready-to-paint image.
        image: crate::render::window::AnimatedImage,
    },
    /// Image fetch failed
    FetchImageFailed {
        /// The URL the image fetch was attempted for.
        url: String,
        /// The failure that aborted the fetch.
        error: MizuError,
    },
    /// Request failed
    Error(MizuError),
}
