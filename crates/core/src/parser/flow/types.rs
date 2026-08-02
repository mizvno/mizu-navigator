//! `ActionContext` (a gate's context: user-gesture vs. no gate) and the
//! internal `TaintOrigin` classification used while propagating taint.

/// Context of an action to determine if it passes a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionContext {
    /// A user gesture event, e.g. click or submit.  Acts as a gate for
    /// cross-origin navigation (gate G1 in `SECURITY-INVARIANTS.md`).
    UserGesture,
    /// A non-interactive trigger, e.g. root timer or network response.
    NonInteractive,
}

/// Why a variable became tainted — used for diagnostic messages (F3).
#[derive(Debug, Clone)]
pub(super) enum TaintOrigin {
    /// Tainted because it receives the response from a network call.
    NetworkResponse { action_desc: String },
    /// Tainted because it is bound from a `$form` field.
    FormField,
    /// Tainted because it was assigned/computed from another tainted variable.
    Propagated { from_var: String },
}
