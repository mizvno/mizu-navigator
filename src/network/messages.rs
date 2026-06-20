//! # `messages` — Unified Communication Channel and Events

#![forbid(unsafe_code)]

use crate::core::types::{StringInterner, Symbol, Value};
use crate::parser::logic::NetworkMethod;
use crate::parser::{Action, MizuFunction};
use rustc_hash::FxHashMap;
use std::collections::HashMap;

/// Complete payload for document reloading.
#[derive(Debug, Clone)]
pub struct ReloadPayload {
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    pub click_actions: HashMap<u32, Action>,
    pub every_actions: HashMap<u32, Action>,
    pub interner: StringInterner,
    pub initial_variables: Vec<(String, Value)>,
    pub url_registry: crate::parser::UrlRegistry,
    /// Domain of the current document (e.g. `"example.com"` for
    /// `mizu://example.com/index.mizu`).  Used by the logic worker to compose
    /// fully-qualified `mizu://` URLs for `api` endpoints at runtime.
    pub document_domain: String,
    /// Computed (derived) variable bindings, in topological order.
    ///
    /// The logic worker re-evaluates these after every mutation whose target
    /// symbol appears in any binding's `depends_on` list.
    pub computed_bindings: Vec<crate::parser::logic::ComputedBinding>,
}

/// A compile-time–validated HTTP network request produced by `GET(…)` / `POST(…)` / etc.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkRequest {
    pub endpoint_symbol: u32,
    pub method: NetworkMethod,
    pub payload: Option<Value>,
    pub path_param: Option<String>,
    pub target_variable: String,
}

/// Declarative runtime actions executed by the Main Thread.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeAction {
    None,
    /// Compile-time–validated HTTP call (unresolved alias form).
    /// The `LogicWorker` resolves the alias in `send_response` and converts this
    /// into [`RuntimeAction::ResolvedCall`] before forwarding to the main thread.
    NetworkCall {
        method: NetworkMethod,
        endpoint_symbol: u32,
        payload: Option<Value>,
        path_param: Option<String>,
        target_variable: String,
    },
    /// A fully-resolved HTTP call with a concrete URL, produced by the
    /// `LogicWorker` after looking up the alias in the `UrlRegistry`.
    /// The main-thread capability watchdog dispatches this as `NetworkCmd::Fetch`.
    ResolvedCall {
        method: String,
        url: String,
        target_variable: String,
    },
    StoreLocal {
        key: String,
        value: Value,
    },
    /// Copies the text content of the DOM node identified by `node_id` to the
    /// system clipboard.  Only accepted when a user-gesture activation flag is
    /// set on the window manager; rejected silently otherwise.
    CopyToClipboard {
        node_id: String,
    },
    GetSystemTime {
        target_variable: String,
    },
    Navigate {
        url: String,
    },
    /// Requests a direct download of a `media` asset to the user's filesystem.
    ///
    /// TODO: integrate native save-dialog (e.g. `rfd`) for actual filesystem write.
    DownloadMedia {
        url: String,
    },
    /// Download request carrying an unresolved compile-time alias.
    ///
    /// Produced by the `download(alias)` built-in in the evaluator.
    /// The `LogicWorker` resolves the alias → URL in `send_response` before
    /// forwarding to the main thread (same pattern as `NetworkCall` → `ResolvedCall`).
    DownloadAlias {
        endpoint_symbol: u32,
    },
}

/// Variables mutated by the LogicWorker in a single cycle.
#[derive(Debug, Clone)]
pub struct StateUpdate {
    pub mutated_variables: Vec<(String, Value)>,
}

/// Events sent from the UI to the LogicWorker.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Click {
        node_id: u32,
    },
    Timer {
        node_id: u32,
    },
    /// Aggregated submission of a form: sends all fields in a single atomic transaction.
    SubmitForm {
        form_node_id: u32,
        fields: FxHashMap<String, Value>,
    },
    /// Updates a variable directly in the worker store (used by GetSystemTime).
    UpdateVariable {
        name: String,
        value: Value,
    },
    /// Complete document reload.
    Reload(Box<ReloadPayload>),
}

/// Aggregated response of the LogicWorker towards the UI.
#[derive(Debug, Clone)]
pub struct WorkerResponse {
    pub state_update: StateUpdate,
    pub runtime_actions: Vec<RuntimeAction>,
}
