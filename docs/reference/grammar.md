# Mizu Language Grammar

> **Status:** Extracted from authoritative parser sources — July 2026.
> Every production is annotated with the implementing Rust source file and
> function.  Constraints that the parser enforces beyond what the EBNF alone
> expresses are listed in **Constraints** callouts beneath each rule.

---

## Contents

1. [Document Structure](#1-document-structure)
2. [Lexical Conventions](#2-lexical-conventions)
3. [urls Block](#3-urls-block)
4. [logic Block](#4-logic-block)
5. [style Block](#5-style-block)
6. [layout Block](#6-layout-block)
7. [Expression Grammar](#7-expression-grammar)
8. [Action Grammar](#8-action-grammar)
9. [Grammar Coverage Notes](#9-grammar-coverage-notes)

---

## 1. Document Structure

**Implementing source:** `src/parser/splitter.rs` — `split_source_with_origin`

```ebnf
document      = { root_item } ;
root_item     = import_directive | block ;

import_directive
              = ( "import" | "include" ) SP+ DQUOTE path DQUOTE NL ;

block          = block_header block_body ;
block_header   = ( "logic" | "style" | "layout" | "urls" ) NL ;

block_body     = { indented_line } ;
indented_line  = SP+ content NL ;
```

`import_directive` may appear anywhere among the root-level items, not only
before the first block — `split_source_with_origin` dispatches on whichever
zero-indent keyword it sees next, so an `import` between two `logic`/`style`
sections is accepted identically to one at the top of the file (see the
`import_can_appear_between_sections` test in `splitter.rs`).

**Constraints (enforced by `split_source_with_origin`):**

- Block header keywords must appear at column 0 and are case-sensitive.
- Blocks may appear in any order; later sections with the same keyword append to the same buffer.
- Blank lines are skipped globally.
- Comments are stripped before dispatch (see §2).
- `import` is forbidden in network-origin documents.
- Import path traversal (`../../`) is rejected at canonicalization time.
- Imported `.mlg`/`.mss` files may not themselves contain `import` directives (one level only).
- Zero-indent non-keyword tokens produce `ParseError`.

---

## 2. Lexical Conventions

### Comments

**Implementing source:** `src/parser/splitter.rs` — `strip_comment`

```ebnf
comment = "//" { any_char } ;
```

`//` is only treated as a comment when it appears at column 0 **or** immediately after ASCII whitespace.  A `//` inside a double-quoted string is never a comment.  This rule preserves `mizu://` URLs in the `urls` block.

### Identifiers

**Implementing source:** `src/parser/logic.rs` — `lex_line`

```ebnf
ident  = ( alpha | "_" | "$" ) { alnum | "_" } ;
alpha  = "A".."Z" | "a".."z" ;
alnum  = alpha | digit ;
digit  = "0".."9" ;
```

`$` prefix is reserved for magic variables (`$form`).

### Numeric Literals

**Implementing source:** `src/parser/logic.rs` — `lex_line` (tokenising), `parse_expr` (literal construction)

```ebnf
num_literal = [ "-" ] digit { digit | "." } ;
```

The lexer scans the literal into an `f64` (`Token::Num`), but **there is no
runtime `Float` type** — `parse_expr` immediately scales every numeric
literal by `DECIMAL_SCALE` (`10_000`), rounds it, and stores it as a single
fixed-point `Value::Int(i64)`.  `4` and `4.0` are both `Value::Int`
internally (`40000` and `40000` respectively, at 4 decimal digits of
precision); there is no distinct `Value` variant that a literal's shape
selects between.  See §3 of `semantics.md` ("Numeric Model") for the full
fixed-point arithmetic model this feeds into.

### String Literals

**Implementing source:** `src/parser/logic.rs` — `lex_line`; `src/parser/layout.rs` — `parse_quoted_string`

```ebnf
string_literal = DQUOTE { str_char } DQUOTE ;
str_char       = escape_seq | <any char except '"'> ;
escape_seq     = "\" <any char> ;
```

Escape sequences: `\\` → `\`, `\"` → `"`, `\{` → `{`, `\}` → `}`. Any `\c` passes `c` through.

### Boolean Literals

```ebnf
bool_literal = "true" | "false" ;
```

### Indentation

Significant whitespace (spaces only). Each block parser determines the *baseline indentation* dynamically from the first non-empty line.

---

## 3. `urls` Block

**Implementing source:** `src/parser/urls.rs` — `parse_urls`

```ebnf
urls_block  = { url_entry } ;
url_entry   = api_entry | media_entry ;
api_entry   = SP+ "api"   SP+ alias SP+ api_path  NL ;
media_entry = SP+ "media" SP+ alias SP+ mizu_url  NL ;
alias       = ident ;
api_path    = "/" { any_char } ;
mizu_url    = "mizu://" { any_char } ;
```

**Constraints:**

| Condition | Error |
|---|---|
| Keyword not `api` or `media` | `ParseError` |
| Missing alias | `ParseError` |
| Missing target | `ParseError` |
| `api` target does not start with `/` | `ParseError` |
| `media` target does not start with `mizu://` | `ParseError` |
| Duplicate alias | `ParseError` |

---

## 4. `logic` Block

**Implementing source:** `src/parser/logic.rs` — `parse_logic`, `parse_root_timers`, `parse_computed`

```ebnf
logic_block  = { logic_item } ;
logic_item   = variable_binding
             | function_def
             | comp_binding
             | root_timer ;

variable_binding = ident "=" expr NL ;

function_def     = inline_function | multiline_function ;
inline_function  = ident "(" param_list ")" ":" expr NL ;
multiline_function
                 = ident "(" param_list ")" NL body_lines ;
body_lines       = { SP+ ident "=" expr NL }
                   SP+ expr NL ;

param_list       = [ param { "," param } ] ;
param            = ident [ ":" type_annotation ] ;
type_annotation  = "num" | "number" | "string" | "str"
                 | "bool" | "boolean" | "list" ;

comp_binding     = SP+ "comp" SP+ ident "=" expr NL ;

root_timer       = SP+ "timer" SP+ interval SP+ "->" SP+ action NL ;
interval         = integer "ms"
                 | number  "s"
                 | integer
                 | ident ;
```

**Constraints:**

- Function names must be unique within the block.
- Recursion (direct or mutual) is rejected at parse time (Kahn's algorithm): `"Recursion and infinite loops are strictly forbidden: a cycle was detected in the function call graph"`.
- `comp` cycles: `"computed variable cycle detected"`.
- `comp` variables are read-only at runtime; assigning to one is `ExecutionError`.
- Maximum expression nesting depth: 256 (`MAX_PARSE_DEPTH`).
- Maximum `comp` bindings per document: 500 (`MAX_COMP_BINDINGS`); exceeding it is `ParseError` naming the actual count and the limit (`parse_computed_with_functions`, checked before cycle detection).
- `timer` and `comp` lines are silently skipped by `parse_logic`; they are handled by separate parsers in additional passes.
- Node-local timers (`every`) are **rejected with `ParseError`**; use root `timer` instead.

---

## 5. `style` Block

**Implementing source:** `src/parser/style.rs` — `parse_style`

```ebnf
style_block    = { style_rule } ;
style_rule     = selector NL { property_line NL } ;
selector       = class_selector | tag_selector ;
class_selector = "." ident ;
tag_selector   = "window" | "box" | "text" | "button"
               | "input"  | "image" | "markdown" ;
property_line  = SP+ property_key SP+ property_value ;
```

**Property table:**

| Key | Value form |
|-----|-----------|
| `width`, `height`, `padding`, `margin`, `gap` | `<number>` or `<number>%` |
| `direction` | `row` \| `column` |
| `justify` | `start` \| `end` \| `center` \| `space-between` \| `space-around` \| `space-evenly` \| `stretch` |
| `align` | `start` \| `end` \| `center` \| `stretch` \| `baseline` |
| `background` | `#rgb` \| `#rrggbb` \| `#rrggbbaa` \| `rgba(r,g,b,a)` \| `linear-gradient(Adeg, #col1, #col2)` |
| `background-image` | relative path (no `://`) |
| `background-size` | `stretch` \| `cover` \| `tile` |
| `color` | hex color |
| `font-size`, `border-radius`, `border-width` | bare number |
| `border-color` | hex color |
| `overflow` | `visible` \| `hidden` \| `scroll` |
| `z-index` | signed integer |
| `display` | `none` \| `flex` |
| `font-family` | `sans-serif` \| `serif` \| `monospace` (fixed 3-generic allowlist — see below) |
| `font-weight` | `normal` \| `bold` \| a bare number `100`–`900` |
| `font-style` | `normal` \| `italic` |
| `text-align` | `left` \| `center` \| `right` \| `justify` |
| `line-height` | bare number (multiplier of font size; default `1.2`) |
| `text-decoration` | `none` \| `underline` |

**Constraints:**

- `:` and `;` in any style line are `ParseError` (CSS syntax rejected).
- `background-image` with `://` in the value is `ParseError` (absolute URLs forbidden).
- Selectors appear at baseline indent; properties must be indented deeper.
- Unknown property keys produce `ParseError`.
- **`font-family` is a fixed allowlist, not a suggestion list.** Only the
  three CSS generics parse; a concrete family name (`"Comic Sans MS"`), a
  URL, or anything resembling `@font-face` is a hard `ParseError`. This is
  deliberate: an arbitrary family string resolved against the OS font
  directory is a fingerprinting surface (which fonts are installed), and any
  path that loads a font from disk or network is a new I/O channel and
  parser attack surface — the same class of concern as the `image src`
  media-alias guard (N4/F1). The author picks a generic; the engine
  guarantees the glyphs via script-aware system font fallback (fontique) —
  see `render::text_engine`'s module doc for the coverage bar and the
  System-only vs. hybrid-bundle determinism decision.

---

## 6. `layout` Block

**Implementing source:** `src/parser/layout.rs` — `parse_layout_with_urls`, `parse_primitive_and_attrs`

```ebnf
layout_block = root_node ;
root_node    = "window" [ inline_text ] attr_list NL { child_line } ;

child_line   = SP+ layout_item NL ;
layout_item  = primitive_node
             | conditional_class
             | event_block ;

primitive_node
             = ( "box" | "text" | "t" | "button" | "input"
               | "image" | "form" | each_node )
               [ inline_text ] attr_list ;
each_node    = "each" SP+ ident SP+ "in" SP+ ident ;
inline_text  = DQUOTE { str_char } DQUOTE ;

attr_list    = { attribute | event_attr } ;
attribute    = attr_key [ "=" ] attr_value ;
attr_key     = ( alnum | "_" | "-" )+ ;
attr_value   = DQUOTE { str_char } DQUOTE | bare_token ;
bare_token   = non_whitespace_char+ ;

event_attr   = ( "click" | "submit" ) "->" action ;

conditional_class
             = "class" SP+ ident SP+ "if" SP+ expr ;
```

**Primitives:**

| Keyword | Alt | Role |
|---------|-----|------|
| `window` | — | Root node (exactly one per document) |
| `box` | — | Layout container |
| `text` | `t` | Text content node |
| `button` | — | Clickable element |
| `input` | — | Form input field |
| `image` | — | Image node |
| `markdown` | — | Rich-text block; inline `"…"` or `"""…"""` multi-line form |
| `each` | — | List iterator |
| `form` | — | Form container |

**Constraints:**

- First non-empty line must be `window`; other roots produce `ParseError`.
- `each` inside another `each` is `ParseError`.
- Absolute URLs (`mizu://`, `http://`, `https://`) in `image src` → `ParseError`, unconditionally (regardless of whether a `urls` registry was supplied).
- `image src` starting with `file://` is `ParseError` when the document is remote-origin (`is_remote_origin = true`).
  ⚠ **Known gap (MNT-01):** a *plain relative path* (e.g. `image src "assets/logo.png"`, matched by `is_direct_path` — anything containing `.` or `/`) in `image src` is currently accepted **without rejection even for remote-origin documents**, contrary to the design this section previously documented ("relative paths in `image src` → `ParseError` for remote-origin documents"). Only the `file://` scheme is actually blocked for remote origin today (`src/parser/layout.rs`, `parse_layout_with_urls`). This is flagged as a suspected parser bug/incomplete implementation, not a deliberate grammar change — see `walkthrough.md`'s "MNT-01" entry. Do not rely on relative-path rejection for remote-origin documents until this is resolved.
- `event_attr` (`->`) consumes the rest of the line; trailing layout attributes are `ParseError`.
- `conditional_class` expressions must be **pure**; effectful calls → `ParseError`.
- `class` attribute values starting with `.` have the dot stripped.
- `bind` attribute → `ParseError` (removed).
- `download -> alias` → `ParseError`; use `click -> download(alias)`.
- `every <interval>` → `ParseError` (node-local timers removed).

---

## 7. Expression Grammar

**Implementing source:** `src/parser/logic.rs` — `parse_expr`, `infix_binding_power`

Parsed with **Pratt (top-down operator precedence)**.  Right BP = left BP + 1 gives left-associativity.

```ebnf
expr        = prefix_expr { infix_op prefix_expr } ;

prefix_expr = atom
            | "-" prefix_expr
            | "!" prefix_expr
            | "if" expr "then" expr "else" expr
            | "(" expr ")" ;

atom        = num_literal
            | bool_literal
            | string_literal
            | ident "(" arg_list ")"
            | ident ;

field_access = expr "." ident ;

arg_list    = [ expr { "," expr } ] ;

infix_op    = "||"
            | "&&"
            | "==" | "!="
            | "<"  | ">"  | "<=" | ">="
            | "+"  | "-"
            | "*"  | "/"
            ;

ternary     = expr "?" expr ":" expr ;
```

**Operator precedence table (low → high):**

| Level | Operator(s) | BP (left, right) | Associativity |
|:---:|---|:---:|:---:|
| 0 | `? :` (ternary) | (0, 0) | right |
| 1 | `\|\|` | (1, 2) | left |
| 2 | `&&` | (3, 4) | left |
| 3 | `==`, `!=` | (5, 6) | left |
| 4 | `<`, `>`, `<=`, `>=` | (7, 8) | left |
| 5 | `+`, `-` | (10, 11) | left |
| 6 | `*`, `/` | (20, 21) | left |
| 7 | Unary `-`, `!` | prefix 30 | — |
| 8 | `.` (field access) | (50, 50) | left |

**Constraints:**

- Maximum nesting depth: **256** (`MAX_PARSE_DEPTH`).
- Unary minus desugars to `0 - operand` in the AST.
- `if/then/else` and `? :` produce the same `IfElse` AST node; only the selected branch is evaluated.
- Field access (`.`) has the highest precedence of any operator.
- `+` on strings performs concatenation; on mismatched types → `TypeError`.
- `&&` and `||` require both operands to be `bool`; other types → `TypeError`.
- Conditional-class expressions must be pure (see §6).

---

## 8. Action Grammar

**Implementing source:** `src/parser/logic.rs` — `parse_action_with_urls`

```ebnf
action        = assignment_action
              | navigate_action
              | network_call
              | download_action
              | eval_action ;

assignment_action = ident "=" expr ;

navigate_action   = "navigate" expr ;

network_call  = http_verb "(" alias
                  [ "," ( payload | path_param ) ]
                  [ "," path_param ]
                ")" "->" ident ;
http_verb     = "GET" | "POST" | "PUT" | "DELETE" | "QUERY" ;
alias         = ident ;
payload       = expr ;
path_param    = expr ;

download_action = "download(" alias ")" ;

eval_action   = expr ;
```

**Network call argument layout:**

| Verb | Args |
|------|------|
| `GET(alias[, path_param]) -> var` | no body |
| `DELETE(alias[, path_param]) -> var` | no body |
| `POST(alias[, payload[, path_param]]) -> var` | body optional |
| `PUT(alias[, payload[, path_param]]) -> var` | body optional |
| `QUERY(alias[, payload[, path_param]]) -> var` | body optional |

**Constraints:**

- Verb keywords are **case-sensitive uppercase**; both the parenthesized call form (`get(alias) -> var`) and the space-separated legacy form (`get /api/foo -> var`) are rejected with a "use the uppercase registry form" `ParseError`.
  *(Resolved MNT-01: `parse_action_with_urls` in `src/parser/logic/parse.rs` previously detected the verb by uppercasing the whole action string before comparing, making the parenthesized form case-insensitive. Fixed to match on exact case; see `walkthrough.md`'s "MNT-01" entry.)*
- `alias` must be declared in the `urls` block as an `api` endpoint; missing or wrong-kind → `ParseError`.
- `download` alias must be declared as a `media` endpoint; wrong kind → `ParseError`.
- `path_param` at runtime must be a single path segment — no `/`, `\`, `..`, or ASCII control characters (`path_param_ok`, `src/parser/logic.rs`).
- Assigning to a `comp` variable in an assignment action → runtime `ExecutionError`.
- `get_system_time(target)` — requests the current time be written to `target`. `target` must be a single **bare variable identifier**, checked at parse time; any other expression (a literal, a field access, a computed expression) → `ParseError`. `target` may not name a `comp` variable (checked at load time by `parser::flow`). This restriction exists so the write destination is always a statically-known `Symbol`, never derived from untrusted data.

---

## 9. Grammar Coverage Notes

### Parser-accepted constructs not expressible in the EBNF

- **`t` shorthand** for `text` primitive: `"t" | "text"` (layout parser only).
- **`markdown` primitive** accepts both an inline quoted string (`markdown "# Hi"`) and a `"""`-delimited multi-line block (`markdown """ … """`), the only primitive with a multi-line inline form.
- **`number` / `str`** as synonyms for `num` / `string` in type annotations.
- **Variable-interval timer:** `timer myVar -> action` (interval read from a runtime variable).

### Removed / reserved constructs (produce `ParseError`)

| Construct | Reason |
|-----------|--------|
| `bind` attribute | Removed; use `class name if condition` |
| `download -> alias` attribute | Removed; use `click -> download(alias)` |
| `every <interval>` on a node | Node-local timers removed permanently |
| `dict`, `record`, `any` type annotations | Not supported; use unannotated parameters |

⚠ **`markdown` primitive was previously documented here as removed — this
was incorrect as of the MNT-01 pass.** `src/parser/layout.rs` still fully
implements it (`Primitive::Markdown`, both inline and `"""`-block forms);
see the primitives table in §6. Corrected in this pass.

### Follow-up: grammar-driven parser

The extracted grammar maps cleanly onto a Pratt expression grammar and an LL(1)
block-structure grammar. Refactoring the hand-rolled parser to be *derived from* this
grammar would eliminate spec-drift at the source. This is noted as a follow-up
in `PROMPT-language-reference.md §Out of scope`.
