//! `parse_action`/`parse_action_with_urls`: the action grammar behind event
//! blocks (`click -> ...`), including built-in HTTP verb calls validated
//! against the compile-time `UrlRegistry`.

use crate::core::errors::MizuError;
use crate::core::types::StringInterner;
use crate::parser::urls::{EndpointKind, UrlRegistry};

use super::super::ast::{Action, Expr, ExprArena, ExprTree, NetworkMethod, PayloadFormat};
use super::super::lexer::{Cursor, Token, assert_cursor_empty, lex};
use super::expr::parse_expr_tree;
use super::helpers::{find_assignment_eq, find_matching_paren, validate_header_name};

/// Parses an action string (e.g. from a `click -> ...` event) into an [`Action`] AST node.
///
/// When `url_registry` is provided, built-in HTTP verb calls (`GET(alias)`,
/// `POST(alias, payload)`, etc.) are compile-time validated against the
/// registry.  Pass `None` to skip validation (e.g., in unit tests).
pub fn parse_action(action: &str, interner: &mut StringInterner) -> Result<Action, MizuError> {
    parse_action_with_urls(action, interner, None)
}

/// Like [`parse_action`] but accepts an optional [`UrlRegistry`] for API guard validation.
pub fn parse_action_with_urls(
    action: &str,
    interner: &mut StringInterner,
    url_registry: Option<&UrlRegistry>,
) -> Result<Action, MizuError> {
    let action_trimmed = action.trim();

    // ── Helper: parse a `VERB(alias, [...]) -> target` HTTP call ──
    //
    // Argument layout depends on whether the verb carries a request body:
    //
    //   No-body verbs  (GET, DELETE):  `(alias[, path_param])  -> var`
    //   Body verbs     (POST, PUT, QUERY): `(alias[, payload[, path_param]]) -> var`
    fn parse_network_call(
        method: NetworkMethod,
        rest: &str,
        interner: &mut StringInterner,
        url_registry: Option<&UrlRegistry>,
    ) -> Result<Action, MizuError> {
        let open = rest.find('(').ok_or_else(|| {
            MizuError::ParseError(format!(
                "network call `{m}` missing `(`: expected `{m}(alias) -> var`",
                m = method.as_str()
            ))
        })?;
        // The matching `)` for `open`, not simply the *last* `)` in `rest`:
        // a trailing `header "<name>" <expr>` clause may itself contain
        // parenthesised sub-expressions (e.g. `header "X-Sum" (a + b)`),
        // which a naive `rfind(')')` would mistake for the call's own
        // closing paren. String literals are tracked so a literal `)`
        // inside a quoted payload string doesn't perturb the depth count.
        let close = find_matching_paren(rest, open).ok_or_else(|| {
            MizuError::ParseError(format!(
                "network call `{m}` missing `)`: expected `{m}(alias) -> var`",
                m = method.as_str()
            ))
        })?;
        let args_str = rest[open + 1..close].trim();
        let after_close = rest[close + 1..].trim();
        let arrow_rhs = if let Some(stripped) = after_close.strip_prefix("->") {
            stripped.trim()
        } else {
            return Err(MizuError::ParseError(format!(
                "network call `{m}` missing `-> target_var` after `)`",
                m = method.as_str()
            )));
        };

        // `-> target_var [as <format-keyword>] [header "<name>" <expr>]*`
        //
        // Tokenised (rather than hand-split) so that `header` clause values
        // can be arbitrary expressions: the Pratt expression parser naturally
        // stops at the next `header`/end-of-input token (no infix operator
        // follows a bare identifier), so clauses compose without ambiguity.
        let rhs_tokens = lex(arrow_rhs)?;
        let mut rhs_cursor = Cursor::new(&rhs_tokens);

        let target_var = match rhs_cursor.next() {
            Some(Token::Ident(name)) => (*name).to_string(),
            other => {
                return Err(MizuError::ParseError(format!(
                    "network call `{}`: expected a target variable name after `->`, got {:?}",
                    method.as_str(),
                    other
                )));
            }
        };

        // Optional trailing `as <keyword>` clause selecting the request
        // payload wire format. Fixed at parse time only — never a runtime
        // expression (see `PayloadFormat`'s doc comment).
        let format = if matches!(rhs_cursor.peek(), Some(Token::Ident("as"))) {
            rhs_cursor.next();
            match rhs_cursor.next() {
                Some(Token::Ident(kw)) => PayloadFormat::from_keyword(kw).ok_or_else(|| {
                    MizuError::ParseError(format!(
                        "network call `{}`: unknown payload format `{}`; \
                         expected one of: json, form, text, yaml, multipart",
                        method.as_str(),
                        kw
                    ))
                })?,
                other => {
                    return Err(MizuError::ParseError(format!(
                        "network call `{}`: expected a payload format keyword after `as`, got {:?}",
                        method.as_str(),
                        other
                    )));
                }
            }
        } else {
            PayloadFormat::Json
        };

        // Zero or more `header "<name>" <expr>` clauses. The name is a
        // parse-time string literal — validated (syntax + reserved-name
        // denylist) here, never a runtime expression; the value is an
        // arbitrary expression, evaluated and stringified at request time.
        let mut headers: Vec<(String, ExprTree)> = Vec::new();
        while matches!(rhs_cursor.peek(), Some(Token::Ident("header"))) {
            rhs_cursor.next();
            let name = match rhs_cursor.next() {
                Some(Token::Str(s)) => s.to_string(),
                other => {
                    return Err(MizuError::ParseError(format!(
                        "network call `{}`: expected a string header name after `header`, got {:?}",
                        method.as_str(),
                        other
                    )));
                }
            };
            validate_header_name(&name, method.as_str())?;
            let value_expr = parse_expr_tree(&mut rhs_cursor, interner)?;
            headers.push((name, value_expr));
        }

        assert_cursor_empty(&rhs_cursor, &format!("network call `{}`", method.as_str()))?;

        // Whether this verb carries a request body (POST, PUT, QUERY do; GET, DELETE do not).
        let has_body = matches!(
            method,
            NetworkMethod::Post | NetworkMethod::Put | NetworkMethod::Query
        );

        // Max 3 slots: alias [, second_arg [, third_arg]]
        let mut arg_parts = args_str.splitn(3, ',').map(str::trim);

        let alias_str = arg_parts.next().filter(|s| !s.is_empty()).ok_or_else(|| {
            MizuError::ParseError(format!(
                "network call `{}` missing alias argument",
                method.as_str()
            ))
        })?;

        let alias_sym = interner.get_or_intern(alias_str);

        // ── Compile-time API guard ────────────────────────────────────────
        if let Some(registry) = url_registry {
            match registry.get(&alias_sym) {
                None => {
                    return Err(MizuError::ParseError(format!(
                        "network call `{}({alias_str})`: alias `{alias_str}` \
                         is not defined in the `urls` block",
                        method.as_str()
                    )));
                }
                Some(ep) if ep.kind != EndpointKind::Api => {
                    return Err(MizuError::ParseError(format!(
                        "network call `{}({alias_str})`: alias `{alias_str}` \
                         is a `media` endpoint, not an `api` endpoint",
                        method.as_str()
                    )));
                }
                _ => {}
            }
        }

        // Parse second and third arguments according to verb class.
        //
        // Body verbs:    slot2 = payload,    slot3 = path_param
        // No-body verbs: slot2 = path_param, slot3 = (disallowed)
        let second = arg_parts.next().filter(|s| !s.is_empty());
        let third = arg_parts.next().filter(|s| !s.is_empty());

        let (payload, path_param) = if has_body {
            // POST/PUT/QUERY(alias[, payload[, path_param]])
            let payload = if let Some(src) = second {
                let tokens = lex(src)?;
                let mut cursor = Cursor::new(&tokens);
                Some(parse_expr_tree(&mut cursor, interner)?)
            } else {
                None
            };
            let path_param = if let Some(src) = third {
                let tokens = lex(src)?;
                let mut cursor = Cursor::new(&tokens);
                Some(parse_expr_tree(&mut cursor, interner)?)
            } else {
                None
            };
            (payload, path_param)
        } else {
            // GET/DELETE(alias[, path_param])  — no body slot
            if third.is_some() {
                return Err(MizuError::ParseError(format!(
                    "network call `{}` does not accept a body argument: \
                     use `{}(alias[, path_param]) -> var`",
                    method.as_str(),
                    method.as_str()
                )));
            }
            let path_param = if let Some(src) = second {
                let tokens = lex(src)?;
                let mut cursor = Cursor::new(&tokens);
                Some(parse_expr_tree(&mut cursor, interner)?)
            } else {
                None
            };
            (None, path_param)
        };

        Ok(Action::NetworkCall {
            method,
            alias_sym,
            payload,
            path_param,
            target_var,
            format,
            headers,
        })
    }

    // ── Detect uppercase HTTP verb built-ins: GET(...), POST(...), etc. ──
    // Matched on exact case: the parenthesized call form is intended to be
    // as case-sensitive as the legacy space-separated form rejected below.
    let network_method = if action_trimmed.starts_with("GET(") {
        Some(NetworkMethod::Get)
    } else if action_trimmed.starts_with("POST(") {
        Some(NetworkMethod::Post)
    } else if action_trimmed.starts_with("PUT(") {
        Some(NetworkMethod::Put)
    } else if action_trimmed.starts_with("DELETE(") {
        Some(NetworkMethod::Delete)
    } else if action_trimmed.starts_with("QUERY(") {
        Some(NetworkMethod::Query)
    } else {
        None
    };

    if let Some(method) = network_method {
        let verb_len = method.as_str().len(); // "GET".len() == 3
        let rest = &action_trimmed[verb_len..]; // starts at `(`
        return parse_network_call(method, rest, interner, url_registry);
    }

    // A verb that matches case-insensitively but not exactly (e.g. `get(...)`,
    // `Get(...)`, `gEt(...)`) is the same case-sensitivity bypass as the
    // legacy lowercase space-separated form below — reject with the same
    // error wording rather than silently falling through as an unrecognized
    // action.
    let upper = action_trimmed.to_ascii_uppercase();
    for verb in &["GET(", "POST(", "PUT(", "DELETE(", "QUERY("] {
        if upper.starts_with(verb) {
            let verb_name = &verb[..verb.len() - 1]; // strip trailing '('
            let typed = &action_trimmed[..verb_name.len()];
            return Err(MizuError::ParseError(format!(
                "lowercase `{typed}` is not a valid action; \
                 use the uppercase registry form: {verb_name}(alias) -> var"
            )));
        }
    }

    // Lowercase HTTP verbs (`get url -> var`) are intentionally rejected.
    // Network calls must use the uppercase registry form: GET(alias) -> var.
    for lc_verb in &["get ", "post ", "put ", "delete "] {
        if action_trimmed.to_ascii_lowercase().starts_with(lc_verb) {
            let verb = lc_verb.trim_end();
            return Err(MizuError::ParseError(format!(
                "lowercase `{verb}` is not a valid action; \
                 use the uppercase registry form: {}(alias) -> var",
                verb.to_ascii_uppercase()
            )));
        }
    }

    // ── `download(alias)` — compile-time validated media download ────────────
    if action_trimmed.starts_with("download(") {
        let close = action_trimmed.rfind(')').ok_or_else(|| {
            MizuError::ParseError("download: missing `)`: expected `download(alias)`".to_string())
        })?;
        let alias = action_trimmed[9..close].trim();
        if alias.is_empty() {
            return Err(MizuError::ParseError(
                "download: alias cannot be empty: expected `download(alias)`".to_string(),
            ));
        }
        let alias_sym = interner.get_or_intern(alias);
        if let Some(registry) = url_registry {
            match registry.get(&alias_sym) {
                None => {
                    return Err(MizuError::ParseError(format!(
                        "download alias `{alias}` is not declared in the `urls` block"
                    )));
                }
                Some(ep) if ep.kind != EndpointKind::Media => {
                    return Err(MizuError::ParseError(format!(
                        "download alias `{alias}` must be a `media` endpoint, not `api`"
                    )));
                }
                _ => {}
            }
        }
        let download_sym = interner.get_or_intern("download");
        let mut arena = ExprArena::new();
        let arg = arena.alloc(Expr::Variable(alias_sym));
        let (args_start, args_len) = arena.push_args(&[arg])?;
        let root = arena.alloc(Expr::FunctionCall {
            name: download_sym,
            args_start,
            args_len,
        });
        return Ok(Action::Eval(ExprTree { arena, root }));
    }

    if let Some(rest) = action_trimmed.strip_prefix("navigate ") {
        let tokens = lex(rest.trim())?;
        let mut cursor = Cursor::new(&tokens);
        let url = parse_expr_tree(&mut cursor, interner)?;
        assert_cursor_empty(&cursor, "`navigate ...`")?;
        Ok(Action::Navigate { url })
    } else if let Some(eq_pos) = find_assignment_eq(action_trimmed) {
        let lhs = action_trimmed[..eq_pos].trim();
        let rhs = action_trimmed[eq_pos + 1..].trim();

        if lhs.is_empty() || rhs.is_empty() {
            return Err(MizuError::ParseError(format!(
                "invalid assignment action: `{action}`"
            )));
        }

        let tokens = lex(rhs)?;
        let mut cursor = Cursor::new(&tokens);
        let expr = parse_expr_tree(&mut cursor, interner)?;
        assert_cursor_empty(&cursor, &format!("`{lhs} = ...`"))?;
        Ok(Action::Assign {
            target: lhs.to_string(),
            expr,
        })
    } else {
        if action_trimmed.is_empty() {
            return Err(MizuError::ParseError("action cannot be empty".to_string()));
        }
        let tokens = lex(action_trimmed)?;
        let mut cursor = Cursor::new(&tokens);
        let expr = parse_expr_tree(&mut cursor, interner)?;
        assert_cursor_empty(&cursor, &format!("`{action_trimmed}`"))?;
        Ok(Action::Eval(expr))
    }
}
