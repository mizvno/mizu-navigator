/// Messages for UI thread isolation communication
pub use mizu_core::messages;
/// OpenNIC DNS resolver — forces all domain lookups through OpenNIC servers
pub mod dns;
/// URI parser for `mizu://`
pub use mizu_core::core::uri;
/// Zero-Touch Vault for credentials
pub mod vault;
/// Asynchronous QUIC worker thread
pub mod worker;
pub use crate::parser::logic::NetworkMethod;
pub use messages::{
    NetworkRequest, ReloadPayload, RuntimeAction, StateUpdate, TabId, UiEvent, WorkerResponse,
};

use crate::core::errors::MizuError;
use crate::render::navigation::NavigationInitiator;

/// Command sent from the UI loop to the networking thread.
#[derive(Debug)]
pub enum NetworkCmd {
    /// Perform a network request
    Fetch {
        /// Tab that issued this command. Echoed back on every result so the
        /// UI can route the response to the document that asked for it, and
        /// only to that one.
        tab: TabId,
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
        /// Optional request payload (POST / PUT / QUERY).  Serialised by the
        /// network worker according to `format` and sent as the HTTP/3
        /// request body.  `None` for body-less methods.
        payload: Option<crate::core::types::Value>,
        /// Request payload wire format, fixed at parse time; selects both the
        /// serialisation and the `Content-Type` header.
        format: crate::parser::logic::PayloadFormat,
        /// Custom request headers: `(name, evaluated_value)` pairs. Names are
        /// fixed at parse time and denylist-checked (see
        /// `parser::logic::parse::validate_header_name`); values are
        /// stringified and validated (fail-closed, no request sent on
        /// failure) by the network worker before the request is built.
        headers: Vec<(String, crate::core::types::Value)>,
    },
    /// Perform a full navigation request
    Navigate {
        /// Tab that issued this command. Echoed back on every result so the
        /// UI can route the response to the document that asked for it, and
        /// only to that one.
        tab: TabId,
        /// The target document's URL.
        url: String,
        /// Who authorised *this* navigation, echoed back on any
        /// [`NetworkResult::NavigationRedirect`] it produces.
        ///
        /// Carried through the worker for the same reason
        /// [`WorkerResponse::gesture`] is carried through the logic worker: the
        /// UI thread cannot reconstruct agency from a result that arrives
        /// asynchronously, and anything it invents instead is agency the
        /// document never earned. See [`NavigationInitiator::redirect_of`].
        initiator: NavigationInitiator,
    },
    /// Fetch an image and cache it
    FetchImage {
        /// Tab that issued this command. Echoed back on every result so the
        /// UI can route the response to the document that asked for it, and
        /// only to that one.
        tab: TabId,
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
        /// Tab that issued this command. Echoed back on every result so the
        /// UI can route the response to the document that asked for it, and
        /// only to that one.
        tab: TabId,
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
        /// Tab this result belongs to — the value echoed from the command.
        /// Never resolve it against the *active* tab: the user may have
        /// switched, or the tab may have closed (in which case the result is
        /// dropped).
        tab: TabId,
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
        /// Tab this result belongs to — the value echoed from the command.
        /// Never resolve it against the *active* tab: the user may have
        /// switched, or the tab may have closed (in which case the result is
        /// dropped).
        tab: TabId,
        /// The variable the fetch result was bound to (`GET(alias) -> var`).
        target_var: String,
        /// The failure that aborted the request.
        error: MizuError,
    },
    /// Navigation succeeded, returning the new source code to parse
    NavigateSuccess {
        /// Tab this result belongs to — the value echoed from the command.
        /// Never resolve it against the *active* tab: the user may have
        /// switched, or the tab may have closed (in which case the result is
        /// dropped).
        tab: TabId,
        /// The navigated-to document's URL.
        url: String,
        /// The raw `.mizu` source fetched from that URL.
        source: String,
    },
    /// A `Navigate` request received a server redirect (3xx).
    ///
    /// **Provenance (invariant N1)**: only emitted by the `Navigate` handler in
    /// the network worker.  `Fetch`, `FetchImage`, and `NetworkRequest` never
    /// emit this variant — they follow same-origin redirects internally and
    /// surface everything else through `FetchFailed` / `FetchImageFailed` (see
    /// `worker::fetch::handle_fetch_subresource_raw`, which is what enforces
    /// this rather than leaving it to each call site's discretion).
    NavigationRedirect {
        /// Tab this result belongs to — the value echoed from the command.
        /// Never resolve it against the *active* tab: the user may have
        /// switched, or the tab may have closed (in which case the result is
        /// dropped).
        tab: TabId,
        /// The redirect target URL to navigate to next.
        new_url: String,
        /// The initiator of the navigation being redirected, echoed unchanged
        /// from [`NetworkCmd::Navigate`]. The UI thread wraps it with
        /// [`NavigationInitiator::redirect_of`] before re-entering the
        /// navigation choke point, so a redirect can only ever carry the agency
        /// the original navigation already had.
        initiator: NavigationInitiator,
    },
    /// Image fetch succeeded, returning decoded image
    FetchImageSuccess {
        /// Tab this result belongs to — the value echoed from the command.
        /// Never resolve it against the *active* tab: the user may have
        /// switched, or the tab may have closed (in which case the result is
        /// dropped).
        tab: TabId,
        /// The URL the image was fetched from (cache key).
        url: String,
        /// The decoded, ready-to-paint image.
        image: crate::render::window::AnimatedImage,
    },
    /// Image fetch failed
    FetchImageFailed {
        /// Tab this result belongs to — the value echoed from the command.
        /// Never resolve it against the *active* tab: the user may have
        /// switched, or the tab may have closed (in which case the result is
        /// dropped).
        tab: TabId,
        /// The URL the image fetch was attempted for.
        url: String,
        /// The failure that aborted the fetch.
        error: MizuError,
    },
    /// Request failed.
    ///
    /// Carries the originating tab for the same routing reason as every other
    /// variant: the error clears that tab's loading flag and lands in its
    /// inspector log.
    ///
    /// `None` for failures that belong to no tab — the worker's own startup
    /// (runtime, QUIC endpoint, TLS config), which happens before any command
    /// is read. Those are surfaced on the active tab because there is nothing
    /// better to attach them to.
    Error(Option<TabId>, MizuError),
}
