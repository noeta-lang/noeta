//! A **type-directed** generator: programs that are well-typed by construction.
//!
//! # What this buys that the syntax generator cannot
//!
//! [`crate::generate`] produces programs that *parse*, and about seven in a hundred also type-check.
//! That was enough to find eight check-vs-run divergences, because those live in the near-misses —
//! the checker being too lenient is only visible on a program that is subtly wrong. It is not
//! enough for the opposite question.
//!
//! **A checker false positive needs a program known to be correct.** If the generator can only say
//! "this parsed", a rejection proves nothing; if it can say "I built this from the typing rules
//! upward", every rejection is a bug. That inverts the oracle, and it is the one direction nothing
//! in this crate could test.
//!
//! It also lifts the ~7% run rate that bounds every execution oracle here. The backend differential,
//! the leak oracle and the tier-1 differential all only learn something from programs that *run*, so
//! their real sample size is a fifteenth of their sweep. Raising the yield raises all of them.
//!
//! # How it stays correct
//!
//! Every expression is generated **for a requested type** rather than generated and then inspected:
//! [`TypedGen::expr_of`] takes a [`Ty`] and can only emit forms that produce it. Bindings carry
//! their type in scope, so a variable is only ever used where its type fits. Functions are declared
//! with a signature and enter the callable set only after their own body — the same acyclicity rule
//! [`crate::generate::GenOptions::terminating`] uses, for the same reason (nothing may recurse).
//!
//! Two language rules constrain it beyond typing, and both are easy to violate silently:
//!
//! - **No shadowing** (E0059). Every binding gets a unique name, so nothing can collide with an
//!   enclosing one or with a declaration.
//! - **No uninferable literals** (E0023). An empty `[]` in an un-annotated immutable binding cannot
//!   be typed, so list literals here are never empty.
//!
//! The check-clean rate is asserted rather than assumed, exactly as the syntax generator's parse
//! rate is: a type-directed generator that drifts into emitting ill-typed programs still *looks*
//! like it is working, and every false-positive test built on it silently becomes vacuous.

use std::fmt::Write as _;

use crate::generate::Entropy;

/// The generator's type universe. Small on purpose — every type here has literals, operators and
/// at least one method, so `expr_of` always has something to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    /// Only ever `List<Int>` / `List<Str>` / … — a list of a scalar, never of a list, so the
    /// element type always has literals to fill it with.
    List(Box<Ty>),
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Int => f.write_str("int"),
            Ty::Float => f.write_str("float"),
            Ty::Bool => f.write_str("bool"),
            Ty::Str => f.write_str("string"),
            Ty::List(e) => write!(f, "List<{e}>"),
        }
    }
}

/// The scalar types, which is what a list element or a fresh binding is drawn from.
const SCALARS: [Ty; 4] = [Ty::Int, Ty::Float, Ty::Bool, Ty::Str];

/// Generate a well-typed Noeta program from `bytes`.
pub fn program(bytes: &[u8]) -> String {
    let mut g = TypedGen {
        e: Entropy::new(bytes),
        out: String::new(),
        indent: 0,
        depth: 0,
        budget: 120,
        scopes: vec![Vec::new()],
        fns: Vec::new(),
        serial: 0,
    };
    g.program();
    g.out
}

struct TypedGen<'a> {
    e: Entropy<'a>,
    out: String,
    indent: usize,
    depth: u32,
    budget: u32,
    /// Bindings in scope, innermost last. Names are globally unique, so this is a lookup table
    /// rather than a shadowing stack — but it is still scoped, because a binding made inside a
    /// block is not in scope after it.
    scopes: Vec<Vec<(String, Ty)>>,
    /// Functions whose bodies are already emitted: `(name, params, return)`. A body may call only
    /// these, so the call graph is acyclic and every program terminates.
    fns: Vec<(String, Vec<Ty>, Ty)>,
    /// Source of unique names.
    serial: u32,
}

impl TypedGen<'_> {
    // ---- plumbing ---------------------------------------------------------------------------

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn newline(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }

    fn may_nest(&self) -> bool {
        self.budget > 0 && self.depth < 3 && !self.e.spent()
    }

    /// A name nothing else uses — the no-shadowing rule (E0059) makes this mandatory, not tidy.
    fn fresh(&mut self, prefix: &str) -> String {
        self.serial += 1;
        format!("{prefix}{}", self.serial)
    }

    fn scalar(&mut self) -> Ty {
        SCALARS[self.e.below(SCALARS.len())].clone()
    }

    /// Any type in the universe: a scalar, or a list of one.
    fn any_ty(&mut self) -> Ty {
        if self.e.chance(25) {
            Ty::List(Box::new(self.scalar()))
        } else {
            self.scalar()
        }
    }

    /// Every in-scope binding of exactly `ty`.
    fn visible_of(&self, ty: &Ty) -> Vec<String> {
        self.scopes
            .iter()
            .flatten()
            .filter(|(_, t)| t == ty)
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn bind(&mut self, name: String, ty: Ty) {
        self.scopes
            .last_mut()
            .expect("at least one scope")
            .push((name, ty));
    }

    // ---- program ----------------------------------------------------------------------------

    fn program(&mut self) {
        let items = 1 + self.e.below(5);
        for _ in 0..items {
            if !self.spend() {
                break;
            }
            if self.e.chance(25) {
                self.fn_decl();
            } else {
                self.stmt();
            }
            self.newline();
        }
    }

    fn block(&mut self, ret: Option<&Ty>) {
        self.push("{");
        self.indent += 1;
        self.scopes.push(Vec::new());
        let stmts = self.e.below(3);
        for _ in 0..stmts {
            if !self.spend() {
                break;
            }
            self.newline();
            self.stmt();
        }
        // A function body ends in `return <expr of the declared type>`, so the signature is honored
        // on every path — there is no early `return` anywhere else in this generator, which is what
        // keeps that true without any flow analysis.
        if let Some(ty) = ret {
            self.newline();
            self.push("return ");
            let ty = ty.clone();
            self.expr_of(&ty);
        }
        self.scopes.pop();
        self.indent -= 1;
        self.newline();
        self.push("}");
    }

    // ---- declarations -----------------------------------------------------------------------

    fn fn_decl(&mut self) {
        let name = self.fresh("f");
        let n_params = self.e.below(3);
        let mut params: Vec<(String, Ty)> = Vec::new();
        for _ in 0..n_params {
            let p = self.fresh("p");
            let t = self.any_ty();
            params.push((p, t));
        }
        let ret = self.any_ty();

        let _ = write!(self.out, "fn {name}(");
        for (i, (p, t)) in params.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            let _ = write!(self.out, "{p}: {t}");
        }
        let _ = write!(self.out, "): {ret} ");

        // The body sees ONLY its parameters: a named function is sealed, so top-level bindings are
        // out of scope inside it unless `use (…)`-captured.
        let saved = std::mem::replace(&mut self.scopes, vec![params.clone()]);
        self.block(Some(&ret));
        self.scopes = saved;

        // Callable only now — nothing already emitted can reach it, so no cycle exists.
        self.fns
            .push((name, params.into_iter().map(|(_, t)| t).collect(), ret));
    }

    // ---- statements -------------------------------------------------------------------------

    fn stmt(&mut self) {
        match self.e.below(6) {
            0..=2 => self.binding(),
            3 => self.echo(),
            4 if self.may_nest() => self.for_stmt(),
            5 if self.may_nest() => self.if_stmt(),
            _ => self.echo(),
        }
    }

    fn binding(&mut self) {
        let ty = self.any_ty();
        let name = self.fresh("v");
        // Annotated half the time. Both spellings must type-check, so this is real coverage of the
        // annotated and inferred binding paths rather than decoration.
        if self.e.chance(50) {
            let _ = write!(self.out, "{name}: {ty} = ");
        } else {
            let _ = write!(self.out, "{name} = ");
        }
        self.expr_of(&ty);
        self.bind(name, ty);
    }

    fn echo(&mut self) {
        self.push("echo ");
        let ty = self.any_ty();
        self.expr_of(&ty);
    }

    fn for_stmt(&mut self) {
        let v = self.fresh("i");
        // A literal range: bounded, so the program terminates by construction.
        let _ = write!(self.out, "for {v} in 0..{} ", 1 + self.e.below(4));
        self.depth += 1;
        self.scopes.push(vec![(v, Ty::Int)]);
        self.block(None);
        self.scopes.pop();
        self.depth -= 1;
    }

    fn if_stmt(&mut self) {
        self.push("if ");
        self.depth += 1;
        // Parenthesized: an `if` head is a *restricted* position — a `{` follows it — so a
        // condition that is itself an `if … then … else` collides with the block's brace and does
        // not parse. `operand_of` brackets it, which is what the formatter prints there too.
        self.operand_of(&Ty::Bool);
        self.push(" ");
        self.block(None);
        if self.e.chance(35) {
            self.push(" else ");
            self.block(None);
        }
        self.depth -= 1;
    }

    // ---- expressions ------------------------------------------------------------------------

    /// Emit an expression of exactly `ty`. Every branch below produces that type or does not exist.
    fn expr_of(&mut self, ty: &Ty) {
        if !self.may_nest() || !self.spend() {
            self.leaf_of(ty);
            return;
        }
        self.depth += 1;
        // A variable of the right type, when one is in scope — the arm that makes the program do
        // something rather than fold constants.
        let vars = self.visible_of(ty);
        let choice = self.e.below(6);
        match choice {
            0 if !vars.is_empty() => {
                let name = vars[self.e.below(vars.len())].clone();
                self.push(&name);
            }
            1 => self.binary_of(ty),
            2 => self.conditional_of(ty),
            3 => self.method_of(ty),
            4 if !self.fns.is_empty() => self.call_of(ty),
            _ => self.leaf_of(ty),
        }
        self.depth -= 1;
    }

    /// A literal of `ty`. Always available, which is what makes generation total.
    fn leaf_of(&mut self, ty: &Ty) {
        match ty {
            Ty::Int => {
                let _ = write!(self.out, "{}", self.e.below(100) as i64);
            }
            Ty::Float => {
                let n = self.e.below(100);
                let _ = write!(self.out, "{n}.5");
            }
            Ty::Bool => {
                let b = self.e.chance(50);
                let _ = write!(self.out, "{b}");
            }
            Ty::Str => {
                let n = self.e.below(1000);
                let _ = write!(self.out, "\"s{n}\"");
            }
            // Never empty: an un-annotated empty list literal is E0023 (nothing determines its
            // element type), and this may land in exactly such a position.
            Ty::List(elem) => {
                self.push("[");
                let n = 1 + self.e.below(3);
                for i in 0..n {
                    if i > 0 {
                        self.push(", ");
                    }
                    let elem = (**elem).clone();
                    self.leaf_of(&elem);
                }
                self.push("]");
            }
        }
    }

    /// An operand of an operator, or the receiver of a method call: parenthesized unless it is a
    /// leaf.
    ///
    /// The generator reasons about the expression *tree* it intends, and prints infix notation —
    /// which re-associates. `xs ~ ys` used as the receiver of `.len()` prints as `xs ~ ys.len()`,
    /// and the `.len()` binds to `ys` alone: a different tree, of a different type, and the program
    /// the checker sees is not the one the generator built. Parentheses are how the intended tree
    /// survives being written down. They cost nothing — `noeta fmt` removes the redundant ones.
    fn operand_of(&mut self, ty: &Ty) {
        // A leaf needs no parentheses and reads better without them, and a bare variable is a leaf
        // too — but neither is known until it is emitted, so this brackets by what it is about to
        // generate rather than by what came out.
        if !self.may_nest() || !self.spend() {
            self.leaf_of(ty);
            return;
        }
        self.push("(");
        self.expr_of(ty);
        self.push(")");
    }

    /// A binary expression whose *result* is `ty`.
    fn binary_of(&mut self, ty: &Ty) {
        match ty {
            // Arithmetic is closed over each numeric type; mixing them is rejected, so both
            // operands are generated at the same type.
            Ty::Int | Ty::Float => {
                let op = ["+", "-", "*"][self.e.below(3)];
                self.operand_of(ty);
                let _ = write!(self.out, " {op} ");
                self.operand_of(ty);
            }
            // A bool comes from comparing two same-typed values, or from combining two bools.
            Ty::Bool => {
                if self.e.chance(50) {
                    let op = ["&&", "||"][self.e.below(2)];
                    self.operand_of(&Ty::Bool);
                    let _ = write!(self.out, " {op} ");
                    self.operand_of(&Ty::Bool);
                } else {
                    let operand = self.scalar();
                    // Ordering is only defined between two values of one type; `==`/`!=` are
                    // universal but still need the two sides to be comparable.
                    let op = ["==", "!=", "<", "<=", ">", ">="][self.e.below(6)];
                    self.operand_of(&operand);
                    let _ = write!(self.out, " {op} ");
                    self.operand_of(&operand);
                }
            }
            // `~` display-concatenates anything into a string.
            Ty::Str => {
                let other = self.scalar();
                self.operand_of(&Ty::Str);
                self.push(" ~ ");
                self.operand_of(&other);
            }
            // `~` on two lists of the same element type concatenates them.
            Ty::List(_) => {
                self.operand_of(ty);
                self.push(" ~ ");
                self.operand_of(ty);
            }
        }
    }

    /// `if c then a else b`, both branches at `ty`.
    fn conditional_of(&mut self, ty: &Ty) {
        self.push("if ");
        self.expr_of(&Ty::Bool);
        self.push(" then ");
        self.expr_of(ty);
        self.push(" else ");
        self.expr_of(ty);
    }

    /// A method call whose return type is `ty`.
    fn method_of(&mut self, ty: &Ty) {
        match ty {
            // `len()` on a list or a string.
            Ty::Int if self.e.chance(50) => {
                let elem = self.scalar();
                self.operand_of(&Ty::List(Box::new(elem)));
                self.push(".len()");
            }
            Ty::Int => {
                self.operand_of(&Ty::Str);
                self.push(".len()");
            }
            Ty::Str => {
                let m = ["upper()", "lower()", "trim()"][self.e.below(3)];
                self.operand_of(&Ty::Str);
                let _ = write!(self.out, ".{m}");
            }
            Ty::List(elem) => {
                let ty = Ty::List(elem.clone());
                self.operand_of(&ty);
                self.push(".reverse()");
            }
            _ => self.leaf_of(ty),
        }
    }

    /// A call to a previously declared function returning `ty`.
    fn call_of(&mut self, ty: &Ty) {
        let candidates: Vec<usize> = self
            .fns
            .iter()
            .enumerate()
            .filter(|(_, (_, _, r))| r == ty)
            .map(|(i, _)| i)
            .collect();
        let Some(&idx) = candidates.get(self.e.below(candidates.len().max(1))) else {
            self.leaf_of(ty);
            return;
        };
        let (name, params, _) = self.fns[idx].clone();
        let _ = write!(self.out, "{name}(");
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.expr_of(p);
        }
        self.push(")");
    }
}
