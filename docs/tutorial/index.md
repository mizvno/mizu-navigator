# Mizu Tutorial

A progressive, hands-on introduction to writing Mizu documents.  Every
code block below is a complete, runnable `.mizu` fragment; they are all
machine-checked by `tests/reference_examples.rs`.

---

## 1. The Minimal Document

Every Mizu document needs at least a `layout` block with a `window` root node.

```
layout
    window "Hello, Mizu!"
        text "Hello, Mizu!"
```

**What this does:**
- `layout` — starts the layout block (zero-indent keyword).
- `window "Hello, Mizu!"` — the single required root node.  The quoted string
  sets the OS window title only — it is **not** rendered as page content.
  (This is different from every other primitive: `box "..."`, `button "..."`,
  etc. all turn their inline string into a visible child `text` node. `window`
  is the one exception, since its string is metadata about the window, not
  document content.)
- `text "Hello, Mizu!"` — the actual visible content; without an explicit
  `text`/`box` child, the window would open with a title but a blank page.

Save this as `hello.mizu` and open it with the navigator.

---

## 2. Adding Structure with `box` and `text`

Use `box` to group elements; `text` (or `t`) for text content.

```
layout
    window "My App"
        box
            text "First paragraph"
            text "Second paragraph"
        box
            text "Another group"
```

Indentation determines the tree structure.  Each extra level of indent = one
level deeper in the DOM tree.

---

## 3. Styling with the `style` Block

```
style
    window
        background #1a1a2e
        color #eaeaea

    .card
        background #16213e
        padding 20
        border-radius 8

layout
    window "Styled App"
        box class card
            text "This is a styled card"
```

- The `style` block maps selectors (`.card`, primitive names) to properties.
- `class card` on a node applies the `.card` rule.
- Properties use `key value` syntax — no `:` or `;`.
- Colors are unquoted hex: `#rrggbb` (or `#rgb`, `#rrggbbaa`).
- Dimensions are bare numbers (pixels) or `50%` (percent).

**Typography:**

```
style
    .headline
        font-family serif
        font-weight bold
        font-style italic
        text-align center
        line-height 1.4
```

- `font-family` accepts only the three CSS generics — `sans-serif`, `serif`,
  `monospace` — never a concrete font name, a URL, or `@font-face`. This is
  a deliberate, fixed allowlist: a concrete family name resolved against the
  OS font directory would be a fingerprinting surface, and loading a font
  from disk or network would be a new, unwanted I/O channel. The author
  picks a generic; the engine renders it correctly in every script it
  encounters (Latin, Cyrillic, Greek, Arabic, Hebrew, Han, Japanese, Korean,
  Devanagari, Bengali, Thai, and emoji, at minimum) via automatic,
  script-aware system font fallback — no `lang`/font-file authoring needed.
- `font-weight` is `normal`, `bold`, or a bare number `100`–`900`.
- `font-style` is `normal` or `italic`.
- `text-align` is `left`, `center`, `right`, or `justify`.
- `line-height` is a multiplier of the font size (default `1.2` when unset).
- `text-decoration` is `none` or `underline`.

---

## 4. Logic: Variables and Functions

The `logic` block defines variables and pure functions.

```
logic
    greeting = "Hello, world!"
    count = 0
    double(x: num) : x * 2

layout
    window "Logic Demo"
        text "{greeting}"
        text "double(5) = {double(5)}"
```

- `greeting = "Hello, world!"` — a variable (zero-argument function).
- `count = 0` — another variable, initial value `0`.
- `double(x: num) : x * 2` — a pure function with one typed parameter. **Note: Type annotations on function parameters are mandatory.** The supported types are: `num` (or `number`), `string` (or `str`), `bool` (or `boolean`), `list<T>`, `record{k: T, ...}`, and optional variants like `T?`.
- `{greeting}` — string interpolation in text content.

---

## 5. Interactive Button: Mutating State

```
logic
    count = 0

layout
    window "Counter"
        text "{count}"
        button "Increment" click -> count = count + 1
```

- `click -> count = count + 1` — an action triggered on click.
- After each click the logic worker increments `count` and the UI re-renders.

---

## 6. Fetching Data from an API

Declare an endpoint in `urls`, then call it from an action.

```
urls
    api items /api/v1/items

logic
    items = null
    loaded = false

layout
    window "Item List"
        button "Load Items" click -> GET(items_api) -> items
        button "Load Items" click -> GET(items) -> items
```

Wait — the alias name must match what is declared.  Let us do it correctly:

```
urls
    api items_api /api/v1/items

logic
    items = null

layout
    window "Item List"
        button "Load Items" click -> GET(items_api) -> items
        text "{items}"
```

When the button is clicked, `GET /api/v1/items` is issued.  When the response
arrives, `items` is updated with the parsed JSON and the text re-renders.

---

## 7. Derived Values with `comp`

`comp` declares a variable whose value is automatically recomputed whenever
any of its dependencies change.

```
logic
    price = 10
    qty = 3
    comp total = price * qty
    comp vat = total * 0.22

layout
    window "Invoice"
        text "Price: {price}"
        text "Qty:   {qty}"
        text "Total: {total}"
        text "VAT:   {vat}"
        button "Add item" click -> qty = qty + 1
```

Clicking "Add item" increments `qty`; the logic worker automatically
recomputes `total` and `vat` in topological order and sends both updates to
the UI.

---

## 8. Rendering a List with `each`

```
urls
    api todos_api /api/v1/todos

logic
    todos = null

layout
    window "To-do List"
        button "Load" click -> GET(todos_api) -> todos
        each todo in todos
            text "{todo.title}"
```

- `each todo in todos` — iterates over the `todos` list variable.
- `{todo.title}` — dot-path interpolation through each record element.
- Nested `each` is not allowed.

---

## 9. Forms

```
urls
    api submit_api /api/v1/message

logic
    result = ""

layout
    window "Contact"
        form submit -> POST(submit_api, $form) -> result
            input type "text" name "message"
            button "Send" type "submit"
        text "{result}"
```

- `form submit -> action` — the form container; action fires on submit.
- `$form` — magic record automatically populated with all named input values.
- `POST(submit_api, $form) -> result` — sends the form as JSON and stores the
  response in `result`.

---

## 10. A Recurring Timer

```
logic
    tick = 0
    timer 1000ms -> tick = tick + 1

layout
    window "Clock"
        text "Seconds elapsed: {tick}"
```

- `timer 1000ms -> action` — fires the action once per second.
- Root timers are declared in the `logic` block; there are no node-level timers.

---

## Next Steps

- Read the complete **grammar** in [`docs/reference/grammar.md`](../reference/grammar.md).
- Read the **semantics** (edge cases, resource bounds) in [`docs/reference/semantics.md`](../reference/semantics.md).
- Read the **security invariants** in [`SECURITY-INVARIANTS.md`](../../SECURITY-INVARIANTS.md).
