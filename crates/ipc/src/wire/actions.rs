//! # `wire::actions` — `WireRuntimeAction`
//!
//! rkyv-archivable mirror of [`mizu_core::messages::RuntimeAction`].
//! These flow **worker → main process** inside a [`WireWorkerEnvelope`]
//! frame and represent capability requests the broker must validate before
//! executing.
//!
//! ## Security note
//!
//! The broker **never** blindly executes a `WireRuntimeAction`.  Every
//! variant is subjected to the capability-broker validation in Phase 3.
//! The type system makes the untrusted origin explicit: a `WireRuntimeAction`
//! coming off the wire is distinct from the `RuntimeAction` used internally
//! by the broker after validation.

#![forbid(unsafe_code)]

use rkyv::{Archive, Deserialize, Serialize};

use crate::wire::value::WireValue;

/// Wire-format mirror of [`mizu_core::messages::NetworkMethod`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug, PartialEq))]
pub enum WireNetworkMethod {
    Get,
    Post,
    Put,
    Delete,
    Query,
}

/// Wire-format mirror of [`mizu_core::parser::logic::PayloadFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug, PartialEq))]
pub enum WirePayloadFormat {
    Json,
    Form,
    Text,
    Yaml,
    Multipart,
}

/// Wire-format mirror of [`mizu_core::messages::RuntimeAction`].
///
/// All header maps are encoded as parallel `Vec`s; see the module-level note
/// in [`crate::wire::reload`] for the rationale.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireRuntimeAction {
    /// No-op placeholder — mirrors `RuntimeAction::None`.
    None,

    /// Unresolved network call: the broker must look up the alias in the
    /// URL registry it retains from the `ReloadPayload`.
    NetworkCall {
        method: WireNetworkMethod,
        /// Raw u32 of the URL alias `Symbol`.
        endpoint_symbol: u32,
        payload: Option<WireValue>,
        path_param: Option<String>,
        /// Raw u32 of the target-variable `Symbol`.
        target_variable_sym: u32,
        format: WirePayloadFormat,
        header_keys:   Vec<String>,
        header_values: Vec<WireValue>,
    },

    /// Already-resolved call: the alias was looked up by the worker before
    /// confinement.  The broker skips the alias-resolution step.
    ResolvedCall {
        /// Uppercase HTTP method string (`"GET"`, `"POST"`, …).
        method: String,
        /// Fully-resolved `mizu://` URL.
        url: String,
        payload: Option<WireValue>,
        /// Raw u32 of the target-variable `Symbol`.
        target_variable_sym: u32,
        format: WirePayloadFormat,
        header_keys:   Vec<String>,
        header_values: Vec<WireValue>,
    },

    /// Persist `key` → `value` to the origin's encrypted local storage.
    StoreLocal {
        key:   String,
        value: WireValue,
    },

    /// Copy the text content of `node_id` to the system clipboard.
    /// Only executed by the broker when the `gesture` flag is set on
    /// `WireWorkerResponse`.
    CopyToClipboard {
        node_id: String,
    },

    /// Request the current UNIX time (ms) and bind it to a variable.
    GetSystemTime {
        target_variable_sym: u32,
    },

    /// Navigate to a new document.  The broker's navigation choke point
    /// (N2) validates the URL and the gesture flag before acting.
    Navigate {
        url: String,
    },

    /// Download a media asset to the user's filesystem.
    DownloadMedia {
        url: String,
    },

    /// Download request carrying an unresolved compile-time alias.
    DownloadAlias {
        endpoint_symbol: u32,
    },
}

// ── Conversions ──────────────────────────────────────────────────────────────

impl From<&mizu_core::messages::RuntimeAction> for WireRuntimeAction {
    fn from(a: &mizu_core::messages::RuntimeAction) -> Self {
        use mizu_core::messages::RuntimeAction;
        match a {
            RuntimeAction::None => WireRuntimeAction::None,
            RuntimeAction::NetworkCall {
                method, endpoint_symbol, payload, path_param,
                target_variable, format, headers,
            } => WireRuntimeAction::NetworkCall {
                method:              WireNetworkMethod::from(method),
                endpoint_symbol:     *endpoint_symbol,
                payload:             payload.as_ref().map(WireValue::from),
                path_param:          path_param.clone(),
                target_variable_sym: target_variable.0,
                format:              WirePayloadFormat::from(format),
                header_keys:   headers.iter().map(|(k, _)| k.clone()).collect(),
                header_values: headers.iter().map(|(_, v)| WireValue::from(v)).collect(),
            },
            RuntimeAction::ResolvedCall {
                method, url, payload, target_variable, format, headers,
            } => WireRuntimeAction::ResolvedCall {
                method:              method.clone(),
                url:                 url.clone(),
                payload:             payload.as_ref().map(WireValue::from),
                target_variable_sym: target_variable.0,
                format:              WirePayloadFormat::from(format),
                header_keys:   headers.iter().map(|(k, _)| k.clone()).collect(),
                header_values: headers.iter().map(|(_, v)| WireValue::from(v)).collect(),
            },
            RuntimeAction::StoreLocal { key, value } => WireRuntimeAction::StoreLocal {
                key:   key.clone(),
                value: WireValue::from(value),
            },
            RuntimeAction::CopyToClipboard { node_id } => {
                WireRuntimeAction::CopyToClipboard { node_id: node_id.clone() }
            }
            RuntimeAction::GetSystemTime { target_variable } => {
                WireRuntimeAction::GetSystemTime { target_variable_sym: target_variable.0 }
            }
            RuntimeAction::Navigate { url } => {
                WireRuntimeAction::Navigate { url: url.clone() }
            }
            RuntimeAction::DownloadMedia { url } => {
                WireRuntimeAction::DownloadMedia { url: url.clone() }
            }
            RuntimeAction::DownloadAlias { endpoint_symbol } => {
                WireRuntimeAction::DownloadAlias { endpoint_symbol: *endpoint_symbol }
            }
        }
    }
}

impl From<&mizu_core::parser::logic::NetworkMethod> for WireNetworkMethod {
    fn from(m: &mizu_core::parser::logic::NetworkMethod) -> Self {
        use mizu_core::parser::logic::NetworkMethod;
        match m {
            NetworkMethod::Get    => WireNetworkMethod::Get,
            NetworkMethod::Post   => WireNetworkMethod::Post,
            NetworkMethod::Put    => WireNetworkMethod::Put,
            NetworkMethod::Delete => WireNetworkMethod::Delete,
            NetworkMethod::Query  => WireNetworkMethod::Query,
        }
    }
}

impl From<&mizu_core::parser::logic::PayloadFormat> for WirePayloadFormat {
    fn from(f: &mizu_core::parser::logic::PayloadFormat) -> Self {
        use mizu_core::parser::logic::PayloadFormat;
        match f {
            PayloadFormat::Json      => WirePayloadFormat::Json,
            PayloadFormat::Form      => WirePayloadFormat::Form,
            PayloadFormat::Text      => WirePayloadFormat::Text,
            PayloadFormat::Yaml      => WirePayloadFormat::Yaml,
            PayloadFormat::Multipart => WirePayloadFormat::Multipart,
        }
    }
}
