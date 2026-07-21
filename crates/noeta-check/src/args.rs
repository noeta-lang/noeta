//! **Argument binding**: matching a written argument list against a declared parameter list.
//!
//! One place answers "which argument supplies which parameter", for every call in the language.
//! Positional arguments bind in order; a `name:` label binds to the parameter it names, wherever
//! that parameter sits.
//!
//! This did not exist. The parser read a label and threw it away, so `add(b: 1, a: 10)` bound
//! positionally — silently computing `a - b` with the operands swapped — and `add(nonsense: 1)`
//! was accepted with no complaint at all. A `#[...]` attribute validated its labels properly the
//! whole time; a call did not, because the information never reached anything that could.

use noeta_ast::CallArg;
use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;

use crate::Checker;

/// Which argument supplies each parameter: `binding[p]` is the index into the written argument
/// list for parameter `p`, or `None` when the parameter is omitted (and must have a default).
pub(crate) type Binding = Vec<Option<usize>>;

impl Checker {
    /// Bind `args` to `param_names`, reporting every label that cannot be honoured.
    ///
    /// Returns the binding, or `None` when the arguments are malformed enough that later checks
    /// would only produce noise (an unknown or duplicated label). A purely positional call is the
    /// identity binding and costs one `any_named` scan.
    pub(crate) fn bind_arguments(
        &mut self,
        args: &[CallArg],
        param_names: &[String],
        callee: &str,
    ) -> Option<Binding> {
        if !CallArg::any_named(args) {
            return Some((0..args.len()).map(Some).collect());
        }

        // A label may not precede a positional argument: `f(a: 1, 2)` gives 2 no position to take,
        // since the labels have already claimed parameters out of order.
        if let Some(bad) = args
            .iter()
            .skip_while(|a| a.name.is_none())
            .find(|a| a.name.is_none())
        {
            self.error(
                DiagnosticCode::InvalidArgument,
                bad.span,
                format!("`{callee}` was given a positional argument after a named one"),
            )
            .help("positional arguments come first; name every argument after the first named one");
            return None;
        }

        let mut binding: Binding = vec![None; param_names.len()];
        let mut ok = true;
        for (i, arg) in args.iter().enumerate() {
            let Some(label) = &arg.name else {
                // Positional: takes the next position, which is its own index because every
                // positional argument precedes every named one.
                if let Some(slot) = binding.get_mut(i) {
                    *slot = Some(i);
                } // an over-long call is an arity error, reported by the caller
                continue;
            };
            let Some(p) = param_names.iter().position(|n| n == label) else {
                let d = self.error(
                    DiagnosticCode::InvalidArgument,
                    arg.span,
                    format!("`{callee}` has no parameter `{label}`"),
                );
                match noeta_diagnostics::closest(label, param_names.iter().map(String::as_str)) {
                    Some(s) => {
                        d.help(format!("did you mean `{s}:`?"));
                    }
                    None if param_names.is_empty() => {
                        d.help(format!("`{callee}` takes no parameters"));
                    }
                    None => {
                        let names: Vec<String> =
                            param_names.iter().map(|n| format!("`{n}:`")).collect();
                        d.help(format!("its parameters are {}", names.join(", ")));
                    }
                }
                ok = false;
                continue;
            };
            if binding[p].is_some() {
                self.error(
                    DiagnosticCode::InvalidArgument,
                    arg.span,
                    format!("`{callee}` was given `{label}` more than once"),
                );
                ok = false;
                continue;
            }
            binding[p] = Some(i);
        }
        ok.then_some(binding)
    }
}

impl Checker {
    /// Normalize a written argument list into **parameter order**, reporting any label that cannot
    /// be honoured, and record the permutation for the backends.
    ///
    /// Returns the reordered arguments, or `None` when the call was rejected. `arg_types` is
    /// permuted in place to stay parallel.
    ///
    /// Doing this once, at the call boundary, is what keeps labels out of the rest of the checker:
    /// generic instantiation, closure finalization and arity checking all continue to see a plain
    /// positional call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn order_arguments(
        &mut self,
        args: &[CallArg],
        param_names: &[String],
        param_types: &[crate::Type],
        required: usize,
        callee: &str,
        arg_types: &mut [crate::Type],
        span: Span,
        call_span: Span,
    ) -> Option<(Vec<CallArg>, Vec<crate::Type>)> {
        let binding = self.bind_arguments(args, param_names, callee)?;

        // A named argument that skips a defaulted parameter (`f(1, c: 9)`) leaves a *hole*. The
        // chosen design is a **supplied mask** on the call — the callee still evaluates the
        // default, over its own upvalues, and the mask tells its prologue which ones to run
        // instead of inferring "the trailing remainder" from a count. `Rvalue::Call::supplied`
        // carries it and lowering computes it; the two backends' prologues do not read it yet, so
        // a hole is still diagnosed here rather than mis-bound.
        let supplied_set: Vec<bool> = binding.iter().map(Option::is_some).collect();
        if let Some(hole) = supplied_set.iter().position(|s| !s)
            && supplied_set[hole..].iter().any(|s| *s)
        {
            self.error(
                DiagnosticCode::InvalidArgument,
                span,
                format!(
                    "`{callee}` would skip parameter `{}`, which is not supported yet",
                    param_names[hole]
                ),
            )
            .help("pass it explicitly for now — skipping a defaulted parameter needs the callee to be told which defaults to run");
            return None;
        }
        // Every parameter without a default must be supplied — by position or by name. Reported
        // here, where the *name* of the missing one is known, rather than as a bare arity count.
        if let Some(p) = (0..required.min(binding.len())).find(|&p| binding[p].is_none()) {
            self.error(
                DiagnosticCode::InvalidArgument,
                span,
                format!("`{callee}` is missing a value for `{}`", param_names[p]),
            );
            return None;
        }

        // Compact to the SUPPLIED parameters, keeping arguments and their parameter types
        // parallel — a skipped parameter must not shift the one after it onto the wrong type.
        let order: Vec<usize> = binding.iter().flatten().copied().collect();
        let supplied_params: Vec<crate::Type> = binding
            .iter()
            .enumerate()
            .filter_map(|(p, b)| b.map(|_| param_types[p].clone()))
            .collect();
        let types: Vec<crate::Type> = order
            .iter()
            .map(|&i| arg_types.get(i).cloned().unwrap_or(crate::Type::Unknown))
            .collect();
        arg_types[..types.len()].clone_from_slice(&types);
        // A call whose parameters are supplied in order, with no gaps, is what lowering already
        // emits — recording it would cost a map entry to say "unchanged".
        let already_positional = binding
            .iter()
            .enumerate()
            .all(|(p, b)| *b == Some(p) || (b.is_none() && p >= order.len()));
        if !already_positional {
            self.sites.arg_orders.insert(call_span, binding);
        }
        let ordered: Vec<CallArg> = order.iter().map(|&i| args[i].clone()).collect();
        Some((ordered, supplied_params))
    }
}
