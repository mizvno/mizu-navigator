import MizuFormal.Types

open Mizu

-- Differential tests verifying that Lean infers the same types as Rust typecheck.rs

def emptyEnv : TyEnv := []

-- Symbol interner (manual assignment for test)
def sym_greeting : Symbol := 0
def sym_count : Symbol := 1
def sym_double : Symbol := 2
def sym_x : Symbol := 3
def sym_form : Symbol := 4

def logic_basics_doc : Doc := {
  functions := [
    (sym_double, {
      params := [(sym_x, .num)],
      body := .binop .mul (.var sym_x) (.lit (.int 2))
    })
  ],
  comps := [],
  timers := [],
  clicks := [],
  submits := [],
  urls := [],
  declared := [sym_greeting, sym_count],
  formSym := sym_form,
  builtinOf := fun _ => none,
  eachSpecs := []
}

def logic_basics_greeting := Expr.lit (.str "Hello, world!")
def logic_basics_count := Expr.lit (.int 0)
def logic_basics_double_body := logic_basics_doc.functions[0]!.2.body

#eval inferType emptyEnv logic_basics_doc logic_basics_greeting
-- Expected: Except.ok (some Ty.str)
#eval inferType emptyEnv logic_basics_doc logic_basics_count
-- Expected: Except.ok (some Ty.num)
#eval inferType [(sym_x, .num)] logic_basics_doc logic_basics_double_body
-- Expected: Except.ok (some Ty.num)

-- A failing typecheck: type mismatch in function call arity
#eval inferType emptyEnv logic_basics_doc (.call sym_double [])
-- Expected: Except.error Err.typeError

-- A successful call to double
#eval inferType emptyEnv logic_basics_doc (.call sym_double [.lit (.int 5)])
-- Expected: Except.ok none -- because calls are statements/top-level evaluated and inferType returns none for success

-- A successful binop (num * str) - intentionally succeeds in Mizu type inference (dynamic evaluation, static return type)
#eval inferType emptyEnv logic_basics_doc (.binop .mul (.lit (.int 2)) (.lit (.str "hello")))
-- Expected: Except.ok (some Ty.num)
