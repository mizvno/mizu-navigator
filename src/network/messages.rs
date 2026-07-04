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
    /// All compiled logic functions, keyed by interned name.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// Click action mappings, keyed by the u32 id of the triggering node.
    pub click_actions: HashMap<u32, Action>,
    /// Timer (`every …`) action mappings, keyed by the u32 id of the owning node.
    pub every_actions: HashMap<u32, Action>,
    /// Submit action mappings, keyed by the u32 id of the node carrying the
    /// `submit -> …` event (typically a `button type "submit"`).
    pub submit_actions: HashMap<u32, Action>,
    /// Actions of root-level `timer <interval> -> <action>` declarations from
    /// the `logic` block, in declaration order.  Fired via
    /// [`UiEvent::RootTimer`] with the matching index.
    pub root_timer_actions: Vec<Action>,
    /// Frozen name ↔ symbol table shared between the UI thread and the worker.
    pub interner: StringInterner,
    /// Non-null global variables at reload time, as `(name, value)` pairs.
    pub initial_variables: Vec<(String, Value)>,
    /// Compile-time endpoint alias table from the document's `urls` block.
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
    /// Interned symbol (as a raw `u32`) of the endpoint alias to resolve.
    pub endpoint_symbol: u32,
    /// HTTP verb for the request.
    pub method: NetworkMethod,
    /// Optional JSON-serialisable request body (POST / PUT / QUERY).
    pub payload: Option<Value>,
    /// Optional path parameter substituted into the endpoint's `{…}` placeholder.
    pub path_param: Option<String>,
    /// Variable name the response is bound to.
    pub target_variable: String,
}

/// Declarative runtime actions executed by the Main Thread.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeAction {
    /// No-op placeholder (e.g. an unresolved alias, or a discarded action).
    None,
    /// Compile-time–validated HTTP call (unresolved alias form).
    /// The `LogicWorker` resolves the alias in `send_response` and converts this
    /// into [`RuntimeAction::ResolvedCall`] before forwarding to the main thread.
    NetworkCall {
        /// HTTP verb for the request.
        method: NetworkMethod,
        /// Interned symbol (as a raw `u32`) of the endpoint alias to resolve.
        endpoint_symbol: u32,
        /// Optional JSON-serialisable request body (POST / PUT / QUERY).
        payload: Option<Value>,
        /// Optional path parameter substituted into the endpoint's `{…}` placeholder.
        path_param: Option<String>,
        /// Variable name the response is bound to.
        target_variable: String,
    },
    /// A fully-resolved HTTP call with a concrete URL, produced by the
    /// `LogicWorker` after looking up the alias in the `UrlRegistry`.
    /// The main-thread capability watchdog dispatches this as `NetworkCmd::Fetch`.
    ResolvedCall {
        /// Uppercase HTTP method (`"GET"`, `"POST"`, …).
        method: String,
        /// Fully-resolved `mizu://` target URL.
        url: String,
        /// Request payload (POST / PUT / QUERY), carried over unchanged from
        /// [`RuntimeAction::NetworkCall`] so the body declared in the document
        /// actually reaches the wire.
        payload: Option<Value>,
        /// Variable name the response is bound to.
        target_variable: String,
    },
    /// Persists `key` → `value` to the current origin's encrypted local storage.
    StoreLocal {
        /// The storage key.
        key: String,
        /// The value to persist.
        value: Value,
    },
    /// Copies the text content of the DOM node identified by `node_id` to the
    /// system clipboard.  Only accepted when a user-gesture activation flag is
    /// set on the window manager; rejected silently otherwise.
    CopyToClipboard {
        /// String id of the DOM node whose text content is copied.
        node_id: String,
    },
    /// Requests the current UNIX time (milliseconds) to be written to a variable.
    GetSystemTime {
        /// Variable name the timestamp is bound to.
        target_variable: String,
    },
    /// Requests a full document navigation.
    Navigate {
        /// The target document's URL.
        url: String,
    },
    /// Requests a direct download of a `media` asset to the user's filesystem.
    ///
    /// TODO: integrate native save-dialog (e.g. `rfd`) for actual filesystem write.
    DownloadMedia {
        /// The absolute `mizu://` URL of the media asset to download.
        url: String,
    },
    /// Download request carrying an unresolved compile-time alias.
    ///
    /// Produced by the `download(alias)` built-in in the evaluator.
    /// The `LogicWorker` resolves the alias → URL in `send_response` before
    /// forwarding to the main thread (same pattern as `NetworkCall` → `ResolvedCall`).
    DownloadAlias {
        /// Interned symbol (as a raw `u32`) of the media endpoint alias.
        endpoint_symbol: u32,
    },
}

/// Variables mutated by the LogicWorker in a single cycle.
#[derive(Debug, Clone)]
pub struct StateUpdate {
    /// `(name, new_value)` pairs for every variable mutated in the last cycle.
    pub mutated_variables: Vec<(String, Value)>,
}

/// Events sent from the UI to the LogicWorker.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A click landed on a node carrying a `click -> …` action.
    Click {
        /// u32 id of the clicked node.
        node_id: u32,
    },
    /// A recurring `every …` timer fired for a node.
    Timer {
        /// u32 id of the node owning the timer.
        node_id: u32,
    },
    /// A root-level `timer …` declared in the `logic` block fired.
    RootTimer {
        /// Index into [`ReloadPayload::root_timer_actions`].
        index: u32,
    },
    /// Aggregated submission of a form: sends all fields in a single atomic transaction.
    SubmitForm {
        /// u32 id of the node whose `submit -> …` action triggered the
        /// submission (the submit button), used to look up the action to
        /// execute after the `$form` record has been populated.
        submitter_node_id: u32,
        /// Form field name → typed value, gathered from every `input` in the form.
        fields: FxHashMap<String, Value>,
    },
    /// Updates a variable directly in the worker store (used by GetSystemTime).
    UpdateVariable {
        /// Name of the variable to update.
        name: String,
        /// The new value.
        value: Value,
    },
    /// Complete document reload.
    Reload(Box<ReloadPayload>),
}

/// Aggregated response of the LogicWorker towards the UI.
#[derive(Debug, Clone)]
pub struct WorkerResponse {
    /// Variables mutated during this cycle.
    pub state_update: StateUpdate,
    /// Capability actions to execute on the main thread.
    pub runtime_actions: Vec<RuntimeAction>,
}
