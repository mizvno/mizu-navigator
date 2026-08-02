//! `Evaluator::evaluate`/`evaluate_impl`: the recursive expression
//! evaluator, including every built-in `FunctionCall` arm (filter/sort/
//! count/length/to_string/contains/has_field/get_system_time/…).

use rustc_hash::FxHashMap;
use std::sync::Arc;

use crate::core::errors::MizuError;
use crate::parser::logic::{
    BinOp, Expr, ExprArena, MizuFunction, apply_binop, check_type, type_name,
};

use super::super::interner::{FrozenInterner, Symbol};
use super::super::value::Value;
use super::compare::compare_values;
use super::types::{Evaluator, MAX_EVAL_DEPTH};

impl Evaluator {
    /// Evaluates a Mizu expression to a concrete Value.
    ///
    /// Budget enforcement is pure-integer: each recursive call increments
    /// `self.instruction_count` and the method returns `Err(MizuError::Timeout)`
    /// once the count exceeds [`MAX_INSTRUCTIONS`].  No hardware clock is read
    /// inside the hot loop — callers must reset `instruction_count` to `0`
    /// before each top-level invocation.
    ///
    /// `eval_depth` guards against native stack overflow on deeply-nested ASTs.
    /// It is incremented on entry and decremented before every return so it is
    /// always consistent; callers do not need to reset it.
    pub fn evaluate(
        &mut self,
        expr: &Expr,
        frame_pointer: usize,
        functions: &FxHashMap<Symbol, MizuFunction>,
        interner: &FrozenInterner,
        arena: &ExprArena,
    ) -> Result<Value, MizuError> {
        self.instruction_count += 1;
        if self.instruction_count > self.max_instructions {
            return Err(MizuError::Timeout);
        }
        self.eval_depth += 1;
        if self.eval_depth > MAX_EVAL_DEPTH {
            self.eval_depth -= 1;
            return Err(MizuError::ExecutionError(
                "evaluation nesting too deep (max 256 levels)".to_owned(),
            ));
        }
        let result = self.evaluate_impl(expr, frame_pointer, functions, interner, arena);
        self.eval_depth -= 1;
        result
    }

    fn evaluate_impl(
        &mut self,
        expr: &Expr,
        frame_pointer: usize,
        functions: &FxHashMap<Symbol, MizuFunction>,
        interner: &FrozenInterner,
        arena: &ExprArena,
    ) -> Result<Value, MizuError> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Variable(sym) => {
                if let Some(val) = self.get_local(*sym, frame_pointer) {
                    Ok(val.clone())
                } else {
                    let val = self.get_global(*sym);
                    if !matches!(val, Value::Null) {
                        Ok(val.clone())
                    } else {
                        let name = interner.resolve(*sym).unwrap_or("<unknown>").to_owned();
                        Err(MizuError::VariableNotFound(name))
                    }
                }
            }
            Expr::BinaryOp { left, op, right } => {
                let lv = self.evaluate(&arena[*left], frame_pointer, functions, interner, arena)?;
                let rv =
                    self.evaluate(&arena[*right], frame_pointer, functions, interner, arena)?;
                apply_binop(
                    op,
                    lv,
                    rv,
                    &mut self.instruction_count,
                    self.max_instructions,
                )
            }
            Expr::FunctionCall {
                name: sym,
                args_start,
                args_len,
            } => {
                let args = arena.args(*args_start, *args_len);
                let resolved_name = interner.resolve(*sym).unwrap_or("<unknown>");
                match resolved_name {
                    "copy_to_clipboard" => {
                        if args.len() != 1 {
                            return Err(MizuError::ExecutionError(
                                "copy_to_clipboard expects 1 argument".to_string(),
                            ));
                        }
                        let val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let node_id = match val {
                            Value::String(ref s) => s.to_string(),
                            _ => {
                                return Err(MizuError::ExecutionError(
                                    "copy_to_clipboard argument must be a node id string"
                                        .to_string(),
                                ));
                            }
                        };
                        self.accumulated_actions
                            .push(crate::messages::RuntimeAction::CopyToClipboard { node_id });
                        return Ok(Value::Bool(true));
                    }
                    "get_system_time" => {
                        // arg[0] must be a bare variable identifier (Expr::Variable),
                        // never evaluated — mirrors `download`'s alias argument.
                        //
                        // Before this restriction, the argument was evaluated as an
                        // arbitrary expression to a string used to *look up* the
                        // write target at runtime, making get_system_time the only
                        // construct in the language whose assignment target was
                        // chosen dynamically rather than fixed at parse time. That
                        // broke the static flow checker's core assumption (every
                        // write target is a known Symbol, `parser::flow.rs`) and put
                        // the write out of reach of taint analysis entirely: a
                        // target string derived from `$form`/a network response
                        // could redirect the write to any variable with no static
                        // check able to see it. Requiring a bare identifier here
                        // fixes the target at parse time, so this is now analyzable
                        // exactly like any other assignment.
                        let target_variable = match args {
                            [id] if matches!(&arena[*id], Expr::Variable(_)) => {
                                let Expr::Variable(sym) = &arena[*id] else {
                                    unreachable!()
                                };
                                *sym
                            }
                            _ => {
                                return Err(MizuError::ExecutionError(
                                    "get_system_time expects a single bare variable \
                                     identifier, e.g. get_system_time(my_var)"
                                        .to_string(),
                                ));
                            }
                        };
                        if self.computed_var_syms.contains(&target_variable) {
                            return Err(MizuError::ExecutionError(
                                "get_system_time cannot target a computed variable".to_string(),
                            ));
                        }
                        self.accumulated_actions.push(
                            crate::messages::RuntimeAction::GetSystemTime { target_variable },
                        );
                        return Ok(Value::Bool(true));
                    }
                    "store_local" => {
                        if args.len() != 2 {
                            return Err(MizuError::ExecutionError(
                                "store_local expects 2 arguments: (key, value)".to_string(),
                            ));
                        }
                        let key_val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let key_str = match key_val {
                            Value::String(ref s) => s.to_string(),
                            _ => {
                                return Err(MizuError::ExecutionError(
                                    "store_local key must be a string".to_string(),
                                ));
                            }
                        };
                        let value = self.evaluate(
                            &arena[args[1]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        self.accumulated_actions
                            .push(crate::messages::RuntimeAction::StoreLocal {
                                key: key_str,
                                value,
                            });
                        return Ok(Value::Bool(true));
                    }
                    "filter" if args.len() == 4 => {
                        let list_val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let field_val = self.evaluate(
                            &arena[args[1]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        // args[2] is always Expr::Literal(Value::String(op)) — the
                        // parser desugars filter's 3-argument surface form to
                        // `op = "eq"` and only ever accepts one of FILTER_OPS as a
                        // bare keyword for the 4-argument form (parser/logic/parse.rs).
                        // Evaluating it here is a formality, never a variable lookup.
                        let op_val = self.evaluate(
                            &arena[args[2]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let target = self.evaluate(
                            &arena[args[3]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let list = match list_val {
                            Value::List(ref l) => l.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("list".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        let field = match field_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        let op = match op_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        // Charge the instruction budget before the native iteration to prevent
                        // large lists from bypassing MAX_INSTRUCTIONS via unmetered CPU work.
                        self.instruction_count =
                            self.instruction_count.saturating_add(list.len() as u64);
                        // `contains` does an O(field length) substring scan per item on
                        // top of the flat per-item charge above — pre-charge that too,
                        // mirroring the standalone `contains` builtin's own charge.
                        if op.as_ref() == "contains" {
                            let extra: u64 = list
                                .iter()
                                .filter_map(|item| {
                                    item.get_field(
                                        crate::core::types::hash_field(field.as_ref()),
                                        field.as_ref(),
                                    )
                                })
                                .filter_map(|v| match v {
                                    Value::String(s) => Some(s.len() as u64),
                                    _ => None,
                                })
                                .sum();
                            self.instruction_count = self.instruction_count.saturating_add(extra);
                        }
                        if self.instruction_count > self.max_instructions {
                            return Err(MizuError::Timeout);
                        }
                        let binop = match op.as_ref() {
                            "eq" => Some(BinOp::Eq),
                            "ne" => Some(BinOp::Ne),
                            "lt" => Some(BinOp::Lt),
                            "le" => Some(BinOp::Le),
                            "gt" => Some(BinOp::Gt),
                            "ge" => Some(BinOp::Ge),
                            "contains" => None,
                            other => {
                                return Err(MizuError::ExecutionError(format!(
                                    "filter: unknown operator `{other}` — expected one of \
                                     eq, ne, lt, le, gt, ge, contains"
                                )));
                            }
                        };
                        let mut ic = self.instruction_count;
                        let mut filtered = Vec::new();
                        for item in list.iter() {
                            if let Some(field_v) = item.get_field(
                                crate::core::types::hash_field(field.as_ref()),
                                field.as_ref(),
                            ) {
                                let include = match &binop {
                                    Some(op) => {
                                        match apply_binop(
                                            op,
                                            field_v.clone(),
                                            target.clone(),
                                            &mut ic,
                                            self.max_instructions,
                                        ) {
                                            Ok(Value::Bool(b)) => b,
                                            Ok(_) => false,
                                            Err(MizuError::TypeError { .. }) => false,
                                            Err(e) => return Err(e),
                                        }
                                    }
                                    None => match (field_v, &target) {
                                        (Value::String(h), Value::String(n)) => {
                                            h.contains(n.as_ref())
                                        }
                                        _ => false,
                                    },
                                };
                                if include {
                                    filtered.push(item.clone());
                                }
                            }
                        }
                        self.instruction_count = ic;
                        return Ok(Value::List(Arc::new(filtered)));
                    }
                    "count" if args.len() == 3 => {
                        let list_val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let field_val = self.evaluate(
                            &arena[args[1]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let target = self.evaluate(
                            &arena[args[2]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let list = match list_val {
                            Value::List(ref l) => l.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("list".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        self.instruction_count =
                            self.instruction_count.saturating_add(list.len() as u64);
                        if self.instruction_count > self.max_instructions {
                            return Err(MizuError::Timeout);
                        }
                        let field = match field_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        // Hashed once, not once per element: the field name is
                        // fixed for the whole pass.
                        let field_hash = crate::core::types::hash_field(field.as_ref());
                        let mut n: i64 = 0;
                        for item in list.iter() {
                            if let Some(v) = item.get_field(field_hash, field.as_ref())
                                && v.budget_eq(
                                    &target,
                                    &mut self.instruction_count,
                                    self.max_instructions,
                                )?
                            {
                                n += 1;
                            }
                        }
                        // An element count is a true integer, not a fixed-point
                        // quantity: `Value::Decimal(n)` would have meant
                        // `n / DECIMAL_SCALE`, i.e. `count(...)` reporting
                        // `0.00000003` for three matches.
                        return Ok(Value::Int(n));
                    }
                    "download" if args.len() == 1 => {
                        // arg[0] must be a bare alias identifier (Expr::Variable);
                        // aliases are not runtime variables and cannot be store-looked-up.
                        let alias_sym = match &arena[args[0]] {
                            Expr::Variable(sym) => *sym,
                            _ => return Err(MizuError::ExecutionError(
                                "download: alias must be a bare identifier, e.g. download(backup)"
                                    .to_string(),
                            )),
                        };
                        self.accumulated_actions.push(
                            crate::messages::RuntimeAction::DownloadAlias {
                                endpoint_symbol: alias_sym.0,
                            },
                        );
                        return Ok(Value::Null);
                    }
                    "sort" if args.len() == 3 => {
                        let list_val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let field_val = self.evaluate(
                            &arena[args[1]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        // args[2] is always Expr::Literal(Value::String("asc"|"desc"))
                        // — the parser accepts nothing else in this position
                        // (parser/logic/parse.rs's parse_sort_call_args), so this is
                        // never a variable lookup and can never be shadowed by an
                        // in-scope variable literally named `asc`/`desc`.
                        let direction_val = self.evaluate(
                            &arena[args[2]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let list = match list_val {
                            Value::List(ref l) => l.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("list".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        let n = list.len();
                        let log2_n = if n > 0 {
                            usize::BITS - n.leading_zeros()
                        } else {
                            0
                        };
                        let sorting_cost = (n as u64).saturating_mul(log2_n as u64);
                        self.instruction_count =
                            self.instruction_count.saturating_add(sorting_cost);
                        if self.instruction_count > self.max_instructions {
                            return Err(MizuError::Timeout);
                        }
                        let field = match field_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        let direction = match direction_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        if direction.as_ref() != "asc" && direction.as_ref() != "desc" {
                            return Err(MizuError::ExecutionError(format!(
                                "sort: direction must be `asc` or `desc`, got `{direction}`"
                            )));
                        }
                        let items: Vec<Value> = (*list).clone();
                        let mut paired = Vec::with_capacity(items.len());
                        // Hashed once for the whole pass, not once per element.
                        let field_hash = crate::core::types::hash_field(field.as_ref());
                        for item in items.into_iter() {
                            let key = item.get_field(field_hash, field.as_ref()).cloned();
                            match key {
                                Some(
                                    Value::Null
                                    | Value::Bool(_)
                                    | Value::Int(_)
                                    | Value::Decimal(_)
                                    | Value::String(_),
                                )
                                | None => {}
                                _ => {
                                    return Err(MizuError::ExecutionError(
                                        "sort: cannot sort on complex nested fields (List/Record)"
                                            .to_string(),
                                    ));
                                }
                            }
                            paired.push((key, item));
                        }
                        paired.sort_by(|(ka, _), (kb, _)| {
                            let ord = compare_values(ka.as_ref(), kb.as_ref());
                            if direction.as_ref() == "desc" {
                                ord.reverse()
                            } else {
                                ord
                            }
                        });
                        let sorted_items = paired.into_iter().map(|(_, v)| v).collect();
                        return Ok(Value::List(Arc::new(sorted_items)));
                    }
                    "length" if args.len() == 1 => {
                        let value = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let n = match value {
                            // O(1): Arc<Vec<Value>>'s length is already tracked.
                            Value::List(ref l) => l.len() as i64,
                            // O(n): char count, not byte count — this is a
                            // user-facing text length for a document-rendering
                            // language, not a security byte-budget (those are
                            // separate, e.g. INPUT_MAX_BYTES), so counting
                            // Unicode scalar values is the correct choice; byte
                            // length would misreport for any multi-byte text.
                            //
                            // The byte length is an O(1) upper bound on the char
                            // count (a UTF-8 string can never have more chars
                            // than bytes), so it's charged and budget-checked
                            // *before* the O(n) `chars().count()` scan runs —
                            // otherwise a huge string (e.g. a large network
                            // response bound to a variable) would pay its full
                            // scan cost before Timeout could ever fire, letting
                            // a single length() call do MAX_INSTRUCTIONS-times
                            // more work than the budget allows.
                            Value::String(ref s) => {
                                let max_possible_chars = s.len() as u64;
                                if self.instruction_count.saturating_add(max_possible_chars)
                                    > self.max_instructions
                                {
                                    return Err(MizuError::Timeout);
                                }
                                let char_count = s.chars().count() as u64;
                                self.instruction_count =
                                    self.instruction_count.saturating_add(char_count);
                                char_count as i64
                            }
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("list or string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        // A length is a true integer — see `count` above.
                        return Ok(Value::Int(n));
                    }
                    "to_string" if args.len() == 1 => {
                        let value = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        // Numbers and bools only: reuses Value's own Display
                        // impl (which already handles DECIMAL_SCALE fixed-point
                        // formatting correctly) rather than reimplementing that
                        // logic. Anything else is a TypeError rather than
                        // silently stringifying — a list/record has no single
                        // canonical textual form, and guessing one invites
                        // confusing output over a clear rejection.
                        match &value {
                            Value::Int(_) | Value::Decimal(_) | Value::Bool(_) => {
                                return Ok(Value::String(Arc::from(value.to_string())));
                            }
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("num or bool".to_string()),
                                    found: type_name(other),
                                });
                            }
                        }
                    }
                    "contains" if args.len() == 2 => {
                        let haystack_val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let needle_val = self.evaluate(
                            &arena[args[1]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let haystack = match haystack_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        let needle = match needle_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        // Charge the scan cost before doing the work — mirrors
                        // the `+` concatenation charge (same discipline: an
                        // O(n) native operation must pre-charge its size).
                        self.instruction_count =
                            self.instruction_count.saturating_add(haystack.len() as u64);
                        if self.instruction_count > self.max_instructions {
                            return Err(MizuError::Timeout);
                        }
                        return Ok(Value::Bool(haystack.contains(needle.as_ref())));
                    }
                    "has_field" if args.len() == 2 => {
                        let record_val = self.evaluate(
                            &arena[args[0]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        let name_val = self.evaluate(
                            &arena[args[1]],
                            frame_pointer,
                            functions,
                            interner,
                            arena,
                        )?;
                        if !matches!(record_val, Value::Record(_)) {
                            return Err(MizuError::TypeError {
                                expected: Box::new("record".to_string()),
                                found: type_name(&record_val),
                            });
                        }
                        let name = match name_val {
                            Value::String(ref s) => s.clone(),
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: Box::new("string".to_string()),
                                    found: type_name(&other),
                                });
                            }
                        };
                        // Additive predicate only — does not relax
                        // Expr::FieldAccess's existing strict-fail-on-missing
                        // behavior; this is for callers who need to branch on
                        // optional/variable-shaped network data before ever
                        // accessing the field, not a replacement for it.
                        let present = record_val
                            .get_field(crate::core::types::hash_field(name.as_ref()), name.as_ref())
                            .is_some();
                        return Ok(Value::Bool(present));
                    }
                    _ => {}
                }

                let func = functions.get(sym).ok_or_else(|| {
                    MizuError::ParseError(format!("call to undefined function `{resolved_name}`"))
                })?;

                if args.len() != func.params.len() {
                    return Err(MizuError::ParseError(format!(
                        "function `{resolved_name}` expects {} argument(s), got {}",
                        func.params.len(),
                        args.len()
                    )));
                }

                let mut evaluated_args = Vec::with_capacity(args.len());
                for &arg_id in args {
                    evaluated_args.push(self.evaluate(
                        &arena[arg_id],
                        frame_pointer,
                        functions,
                        interner,
                        arena,
                    )?);
                }

                let new_fp = self.local_stack.len();
                for ((param_sym, expected_type), arg_val) in func.params.iter().zip(evaluated_args)
                {
                    let param_name = interner.resolve(*param_sym).unwrap_or("<unknown>");
                    check_type(&arg_val, expected_type, resolved_name, param_name)?;
                    self.push_local(*param_sym, arg_val);
                }

                let res = self.evaluate(
                    func.body.root(),
                    new_fp,
                    functions,
                    interner,
                    &func.body.arena,
                );
                self.truncate_locals(new_fp);
                res
            }
            Expr::Let {
                name: sym,
                value,
                body,
            } => {
                let bound_val =
                    self.evaluate(&arena[*value], frame_pointer, functions, interner, arena)?;
                self.push_local(*sym, bound_val);
                let res = self.evaluate(&arena[*body], frame_pointer, functions, interner, arena);
                self.pop_local();
                res
            }
            Expr::Not(inner) => {
                let val =
                    self.evaluate(&arena[*inner], frame_pointer, functions, interner, arena)?;
                match val {
                    Value::Bool(b) => Ok(Value::Bool(!b)),
                    other => Err(crate::core::errors::MizuError::TypeError {
                        expected: Box::new("bool".to_string()),
                        found: type_name(&other),
                    }),
                }
            }
            // Lazy: only the selected branch is evaluated.
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                let cond_val = self.evaluate(
                    &arena[*condition],
                    frame_pointer,
                    functions,
                    interner,
                    arena,
                )?;
                match cond_val {
                    Value::Bool(true) => self.evaluate(
                        &arena[*then_expr],
                        frame_pointer,
                        functions,
                        interner,
                        arena,
                    ),
                    Value::Bool(false) => self.evaluate(
                        &arena[*else_expr],
                        frame_pointer,
                        functions,
                        interner,
                        arena,
                    ),
                    other => Err(crate::core::errors::MizuError::TypeError {
                        expected: Box::new("bool".to_string()),
                        found: type_name(&other),
                    }),
                }
            }
            Expr::FieldAccess {
                base,
                field,
                field_hash,
            } => {
                let base_val =
                    self.evaluate(&arena[*base], frame_pointer, functions, interner, arena)?;
                if !matches!(base_val, Value::Record(_)) {
                    return Err(MizuError::TypeError {
                        expected: Box::new("record".to_string()),
                        found: type_name(&base_val),
                    });
                }
                let field_str = interner.resolve(*field).unwrap_or("");
                base_val
                    .get_field(*field_hash, field_str)
                    .cloned()
                    .ok_or_else(|| MizuError::VariableNotFound(field_str.to_string()))
            }
        }
    }
}
