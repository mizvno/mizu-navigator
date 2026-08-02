//! Layout data types: `Primitive`, `MizuNode`, `EventBlock`, `ConditionalClass`,
//! and the `MAX_LAYOUT_DEPTH` guard.

use rustc_hash::FxHashMap;

use crate::parser::logic::{Action, ExprTree};

/// Maximum nesting depth accepted for a `layout` block's indentation
/// hierarchy (the root `doc` node counts as depth 1).
///
/// The parser itself is iterative — an indentation stack, not recursion — so
/// it has no native depth limit of its own. But the DOM it produces is walked
/// recursively, once per level, by every downstream consumer that has to
/// visit every node: `render::layout_bridge::build_taffy_tree`, taffy's own
/// layout pass, and `render::vello_pipeline::paint_node` all recurse on the
/// UI thread with no depth counter of their own. A hostile document nested
/// far enough (attacker-controlled indentation, well within
/// `MAX_RESPONSE_BODY_BYTES`) would commit successfully and then blow the
/// main-thread stack on the first layout/paint pass — a full-process crash,
/// not a per-document failure. Capping depth here, at parse time, rejects
/// such a document before it ever reaches those walkers. 256 matches
/// [`crate::core::types::eval::MAX_EVAL_DEPTH`]: far beyond any depth a
/// legitimate hand-authored or generated document would use.
pub(super) const MAX_LAYOUT_DEPTH: usize = 256;

/// The valid structural primitives in Mizu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    /// Root document node. Was named `Window` before the `doc` keyword
    /// rename — kept as a plain, OS-window-agnostic document root so the
    /// name stays accurate if Mizu is ever embedded somewhere that doesn't
    /// own a native OS window.
    Doc,
    /// Structural container.
    Box,
    /// Text leaf or block.
    Text,
    /// Interactive button.
    Button,
    /// Input field.
    Input,
    /// Media leaf.
    Image,
    /// Rich text markdown block.
    Markdown,
    /// List iterator: `each item in list`.
    Each,
    /// A form container that batches input values and submits them atomically.
    /// Recognised attributes: `submit -> action`.
    Form,
    /// A section heading, `h1` through `h6`. All six spellings share this one
    /// variant; the level (1-6) is stored as the `"level"` string attribute
    /// rather than as enum payload, matching how `class`/`id`/`dir` are
    /// already represented. No default visual styling — document authors
    /// style headings themselves via `h1`-`h6` tag selectors.
    Heading,
}

impl Primitive {
    /// Returns the string representation of the primitive.
    pub fn as_str(&self) -> &'static str {
        match self {
            Primitive::Doc => "doc",
            Primitive::Box => "box",
            Primitive::Text => "text",
            Primitive::Button => "button",
            Primitive::Input => "input",
            Primitive::Image => "image",
            Primitive::Markdown => "markdown",
            Primitive::Each => "each",
            Primitive::Form => "form",
            Primitive::Heading => "heading",
        }
    }
}

/// Represents a single node in the Mizu DOM tree.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct MizuNode {
    /// The primitive type of this node.
    pub primitive: Primitive,
    /// Inline attributes mapping (e.g. `class -> .card`).
    pub attributes: FxHashMap<String, String>,
    /// Behavioral event blocks mapping (e.g. `click -> EventBlock::Click`).
    pub events: FxHashMap<String, EventBlock>,
    /// For `each` nodes: `(item_variable, list_name)`, e.g. `each item in list` → `("item", "list")`.
    pub iterator_context: Option<(String, String)>,
    /// Runtime-evaluated conditional classes (applied in declaration order after the base class).
    pub conditional_classes: Vec<ConditionalClass>,
}

impl MizuNode {
    /// The tag name a bare (undotted) style selector matches this node
    /// against. Identical to `self.primitive.as_str()` for every primitive
    /// except `Heading`: `h1`-`h6` are six spellings of that one variant, so
    /// they can't be told apart by `as_str()` alone -- this reads the parsed
    /// `level` attribute back out to reconstruct the specific `h1`..`h6` key
    /// a style selector was written against.
    pub fn style_tag_name(&self) -> std::borrow::Cow<'_, str> {
        if self.primitive == Primitive::Heading {
            let level = self
                .attributes
                .get("level")
                .map(String::as_str)
                .unwrap_or("1");
            std::borrow::Cow::Owned(format!("h{level}"))
        } else {
            std::borrow::Cow::Borrowed(self.primitive.as_str())
        }
    }
}

/// A behavioral event block attached to a node.
///
/// Note: there is intentionally no node-local timer. Recurring behaviour is
/// declared exclusively as a root `timer` in the `logic` block, so a
/// document's entire temporal surface is enumerable without walking the layout
/// tree (and cannot be multiplied by `each`).
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum EventBlock {
    /// Triggered on click (e.g. `click -> Redirect("/home")`).
    Click {
        /// Action payload or destination.
        action: Action,
    },
    /// Triggered on form submit (e.g. `submit -> SendForm`).
    Submit {
        /// Action payload to execute on submission.
        action: Action,
    },
}

/// A runtime-evaluated class binding declared as a child line of a node.
///
/// Two independent forms share this type because they are declared with the
/// same `class ...` child-line syntax, but they mean different things at
/// paint time — see each variant.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum ConditionalClass {
    /// `class <name> if <boolean-expr>` — a fixed class name, toggled on or
    /// off. If `condition` evaluates to `true` on a given paint frame,
    /// `class_name` is added to the node's active class set for that frame
    /// (after the static base class); otherwise it contributes nothing.
    Toggle {
        /// CSS class name to activate when the condition is truthy.
        class_name: String,
        /// Pure boolean expression evaluated at runtime (no side effects allowed).
        condition: ExprTree,
    },
    /// `class <condition> ? "<name-a>" : "<name-b>"` (nested ternaries
    /// allowed in either branch) — always contributes exactly one class
    /// name; *which* one depends on evaluating `expr`. Every leaf `expr` can
    /// evaluate to (every branch reachable without passing through a nested
    /// ternary's own condition) is statically guaranteed to be a string
    /// literal — enforced at parse time, not just by convention — so
    /// evaluating this can never itself become an information-flow
    /// concern: the *set* of possible outputs is fixed and known before the
    /// document ever runs, only *which one* is chosen varies.
    Ternary {
        /// Expression tree whose root is `Expr::IfElse`; every leaf is a
        /// string literal.
        expr: ExprTree,
    },
}
