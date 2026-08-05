//! # `broker` — Phase 3: the Main Process as Capability Broker
//!
//! [`execute_capability_action`](super::execute_capability_action) already
//! enforces policy for actions produced by the in-process, same-trust-domain
//! `LogicWorker` **thread**. That trust model breaks the moment `LogicWorker`
//! becomes a sandboxed, out-of-process `mizu-worker` (Phase 1/2): a
//! compromised worker can no longer be trusted to have honestly resolved a
//! `NetworkCall` alias against the document's `urls` block, or to have told
//! the truth about whether a `Navigate` was actually triggered by a user
//! gesture. Both of those computations happen worker-side today
//! (`resolve_endpoint_url` in `mizu_core::parser::logic_worker::helpers`),
//! which is fine when the worker *is* trusted and becomes a forgeable claim
//! the instant it isn't.
//!
//! [`authorize_action`] is the re-validation gate this module adds: every
//! `RuntimeAction` arriving from a sandboxed worker passes through it before
//! it may reach [`execute_capability_action`]. It never trusts a
//! worker-supplied URL, a worker-supplied resolved alias, or a
//! worker-supplied gesture flag — it re-derives each fact from data the
//! broker alone holds (its own copy of the [`UrlRegistry`] loaded from the
//! compiled document, and the [`UiEvent`] the broker itself dispatched).
//!
//! Actions produced by the trusted in-process thread skip re-validation
//! entirely ([`ActionOrigin::TrustedThread`]): they already went through
//! exactly this resolution, just on the worker side, and re-deriving it here
//! is unnecessary and (for `Navigate`) actively wrong, so behavior for that
//! path is completely unchanged from before this module existed.

use mizu_core::core::errors::MizuError;
use mizu_core::core::types::Symbol;
use mizu_core::parser::{UrlRegistry, resolve_endpoint_url};

use crate::network::{RuntimeAction, UiEvent};

/// Whether the event that produced an action carried real user agency.
///
/// Gate G1's token, extracted from the [`UiEvent`] the broker itself
/// dispatched — never from anything the worker reports.
///
/// # Why this is a separate type
///
/// [`authorize_action`] used to take the whole `&UiEvent`. That works when
/// authorization happens synchronously, right where the event is still in
/// hand. The asynchronous bridge cannot do that: a reply arrives later, on a
/// different thread, and correlating it with its event means *keeping* the
/// event alive in a queue — which for `UiEvent::Reload` would mean holding
/// (or cloning) an entire compiled document per in-flight event.
///
/// Reducing the event to this two-valued verdict at the moment it is sent
/// keeps the queue cheap and, more importantly, makes the security property
/// explicit: agency is decided once, at the send site, from the variant the
/// broker chose. Nothing downstream can widen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAgency {
    /// A real user gesture: `Click` or `SubmitForm`, emitted only by
    /// `dispatch_click_gesture` / `dispatch_form_submit` in response to a
    /// mouse click, keyboard activation, or form submission.
    UserGesture,
    /// Document-driven: a timer tick, a network response, a document load.
    /// None of these may navigate.
    DocumentDriven,
}

impl EventAgency {
    /// Derives the agency of `event`.
    ///
    /// The event variant *is* the agency — see the runtime half of gate G1
    /// in `TabSession::apply_event`, which makes the identical distinction
    /// for the in-process path.
    #[must_use]
    pub fn of(event: &UiEvent) -> Self {
        match event {
            UiEvent::Click { .. } | UiEvent::SubmitForm { .. } => EventAgency::UserGesture,
            UiEvent::RootTimer { .. }
            | UiEvent::UpdateVariable { .. }
            | UiEvent::Reload(_)
            | UiEvent::CloseTab => EventAgency::DocumentDriven,
        }
    }

    /// Whether this agency satisfies gate G1.
    #[must_use]
    pub fn is_user_gesture(self) -> bool {
        matches!(self, EventAgency::UserGesture)
    }
}

/// Where a [`RuntimeAction`] originated, and therefore how much the broker
/// may trust the values it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOrigin {
    /// The same-process `LogicWorker` thread (today's architecture).
    /// Already trusted; [`authorize_action`] passes these through unchanged.
    TrustedThread,
    /// A sandboxed `mizu-worker` process, reachable only over the Phase 1/2
    /// IPC transport. Assumed compromised: every resource reference it
    /// claims is independently re-resolved or rejected.
    SandboxedIpcWorker,
}

/// Re-validates one [`RuntimeAction`] against broker-held policy, returning
/// the action the broker is willing to execute (which may differ from the
/// input — e.g. a `NetworkCall` is turned into a broker-resolved
/// `ResolvedCall`) or a [`MizuError::SecurityViolation`] explaining the
/// rejection.
///
/// Call this — for actions sourced from [`ActionOrigin::SandboxedIpcWorker`]
/// — before passing the result to
/// [`execute_capability_action`](super::execute_capability_action). For
/// [`ActionOrigin::TrustedThread`] it is a no-op passthrough, so it is always
/// safe to call unconditionally at the dispatch site.
///
/// # Policy summary
///
/// | Action | Untrusted-worker handling |
/// |---|---|
/// | `NetworkCall { endpoint_symbol, .. }` | Alias resolved against the broker's own `registry`; the worker's claim about the URL is never used. Unknown alias → rejected. |
/// | `ResolvedCall { .. }` | **Always rejected.** A pre-resolved call can only have been forged; only `NetworkCall` (broker-resolved) is accepted from this origin. |
/// | `DownloadAlias { endpoint_symbol }` | Same alias-resolution treatment as `NetworkCall`. |
/// | `DownloadMedia { .. }` | **Always rejected**, mirroring `ResolvedCall`. |
/// | `Navigate { .. }` | Requires `agency` to be [`EventAgency::UserGesture`] (gate G1), derived by the broker from the event it sent — a worker-reported gesture flag is never consulted. |
/// | `StoreLocal`, `CopyToClipboard`, `GetSystemTime`, `None` | Passed through unchanged: already safe by construction (quota/domain enforcement happens downstream and consults no worker-supplied trust claim; clipboard is gated by its own upstream intercept). |
pub fn authorize_action(
    action: RuntimeAction,
    origin: ActionOrigin,
    document_domain: &str,
    registry: &UrlRegistry,
    agency: EventAgency,
) -> Result<RuntimeAction, MizuError> {
    if origin == ActionOrigin::TrustedThread {
        return Ok(action);
    }

    match action {
        RuntimeAction::ResolvedCall { url, .. } => Err(MizuError::SecurityViolation(format!(
            "sandboxed worker sent a pre-resolved network call to `{url}`; only \
             NetworkCall (broker-resolved against the compile-time UrlRegistry) \
             is accepted from an untrusted worker"
        ))),
        RuntimeAction::DownloadMedia { url } => Err(MizuError::SecurityViolation(format!(
            "sandboxed worker sent a pre-resolved media download for `{url}`; only \
             DownloadAlias (broker-resolved against the compile-time UrlRegistry) \
             is accepted from an untrusted worker"
        ))),
        RuntimeAction::NetworkCall {
            method,
            endpoint_symbol,
            payload,
            path_param,
            target_variable,
            format,
            headers,
        } => {
            let endpoint = lookup_endpoint(registry, endpoint_symbol)?;
            let url = resolve_endpoint_url(document_domain, endpoint, path_param.as_deref())?;
            Ok(RuntimeAction::ResolvedCall {
                method: method.as_str().to_owned(),
                url,
                payload,
                target_variable,
                format,
                headers,
            })
        }
        RuntimeAction::DownloadAlias { endpoint_symbol } => {
            let endpoint = lookup_endpoint(registry, endpoint_symbol)?;
            Ok(RuntimeAction::DownloadMedia {
                url: endpoint.raw_target.clone(),
            })
        }
        RuntimeAction::Navigate { url } => {
            if agency.is_user_gesture() {
                Ok(RuntimeAction::Navigate { url })
            } else {
                Err(MizuError::SecurityViolation(format!(
                    "sandboxed worker requested Navigate to `{url}` without gate G1: \
                     the event the broker sent was {agency:?}, not a Click or \
                     SubmitForm user gesture"
                )))
            }
        }
        other => Ok(other),
    }
}

/// Looks up `endpoint_symbol` in the broker's own [`UrlRegistry`], failing
/// closed (never silently substituting a default or a best-effort guess) if
/// the alias is not declared in the document's `urls` block.
fn lookup_endpoint(
    registry: &UrlRegistry,
    endpoint_symbol: u32,
) -> Result<&mizu_core::parser::UrlEndpoint, MizuError> {
    registry.get(&Symbol(endpoint_symbol)).ok_or_else(|| {
        MizuError::SecurityViolation(format!(
            "sandboxed worker referenced endpoint alias {endpoint_symbol}, which is \
             not declared in this document's urls block"
        ))
    })
}

#[cfg(test)]
mod tests;
