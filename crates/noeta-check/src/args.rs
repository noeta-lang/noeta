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

/// The same arguments with their labels cleared — a recovery list for a call already diagnosed,
/// so nothing downstream reports a *second* complaint about the labels.
fn strip_labels(args: &[CallArg]) -> Vec<CallArg> {
    args.iter()
        .map(|a| CallArg {
            name: None,
            ..a.clone()
        })
        .collect()
}

impl Checker {
    /// Bind `args` to `param_names`, reporting every label that cannot be honoured.
    ///
    /// Returns the binding, or `None` when the arguments are malformed enough that later checks
    /// would only produce noise (an unknown or duplicated label). A purely positional call is the
    /// identity binding and costs one `any_named` scan.
    ///
    /// `piped` marks a call desugared from a pipeline, whose *first* argument is the value from
    /// the left of the `|>` — see [`Self::bind_positional`] for what that changes.
    pub(crate) fn bind_arguments(
        &mut self,
        args: &[CallArg],
        param_names: &[String],
        callee: &str,
        piped: bool,
    ) -> Option<Binding> {
        if !CallArg::any_named(args) {
            // No label claims anything, so both binding rules agree: argument `i` supplies
            // parameter `i`, piped or not.
            return Some((0..args.len()).map(Some).collect());
        }

        // A label may not precede a positional argument: `f(a: 1, 2)` gives 2 no position to take,
        // since the labels have already claimed parameters out of order. The piped argument is
        // exempt — it is always first, and is never one the author could have written after a
        // label.
        if let Some(bad) = args
            .iter()
            .skip(usize::from(piped))
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

        // Labels first: each claims the parameter it names, wherever that parameter sits. The
        // positional arguments then take what is left, which is the only order that lets the piped
        // value find its parameter.
        let mut binding: Binding = vec![None; param_names.len()];
        let mut ok = true;
        for (i, arg) in args.iter().enumerate() {
            let Some(label) = &arg.name else { continue };
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
        self.bind_positional(args, param_names, callee, piped, &mut binding, &mut ok);
        ok.then_some(binding)
    }

    /// Place the **positional** arguments into `binding`, which already holds every label's claim.
    ///
    /// The two rules differ in what "this argument's parameter" means:
    ///
    /// - A written positional argument takes the parameter at **its own index** — the author chose
    ///   that position, so a label that already claimed the same parameter is the "given `x` more
    ///   than once" error, reported at the label.
    /// - The **piped** value has no written position at all: `x |> f(…)` says only that `x` is an
    ///   argument of `f`, so it takes the first parameter no label claimed, and the RHS's own
    ///   positionals follow it into the parameters still free. With no labels this is exactly
    ///   "piped value first, written arguments after" — the pipe's behaviour before labels bound —
    ///   so nothing that does not use a label through a pipe changes meaning.
    fn bind_positional(
        &mut self,
        args: &[CallArg],
        param_names: &[String],
        callee: &str,
        piped: bool,
        binding: &mut Binding,
        ok: &mut bool,
    ) {
        let positions = |args: &[CallArg]| -> Vec<usize> {
            (0..args.len())
                .filter(|&i| args[i].name.is_none())
                .collect()
        };
        if piped {
            let unclaimed: Vec<usize> = (0..param_names.len())
                .filter(|&p| binding[p].is_none())
                .collect();
            // Zipping stops at the shorter side: more positionals than free parameters is an
            // over-long call, an arity error the caller reports against the argument count.
            for (i, p) in positions(args).into_iter().zip(unclaimed) {
                binding[p] = Some(i);
            }
            return;
        }
        for i in positions(args) {
            match binding.get(i) {
                Some(None) => binding[i] = Some(i),
                // Claimed by a label already: report it where the label is, naming it.
                Some(Some(claimed)) => {
                    let label = args[*claimed].name.clone().unwrap_or_default();
                    self.error(
                        DiagnosticCode::InvalidArgument,
                        args[*claimed].span,
                        format!("`{callee}` was given `{label}` more than once"),
                    );
                    *ok = false;
                }
                // An over-long call is an arity error, reported by the caller.
                None => {}
            }
        }
    }
}

impl Checker {
    /// Reject every label on `args`, for a callee that has no parameter names to bind one against.
    ///
    /// Reached from [`Checker::check_args`], after any callee that *can* bind has already consumed
    /// its labels. What is left declares no parameter names:
    ///
    /// - A **native function whose registry signature is unnamed** (`ExtFn::param_names` empty).
    ///   Naming a signature is what opts it into labelled calls, so named and unnamed native
    ///   functions coexist and this is the unnamed half.
    /// - A **function value**, whose [`crate::Type::Fn`] carries parameter types only. The closure
    ///   it came from had names, but the type it flows through does not, so the call site cannot
    ///   see them — no registry entry can fix this one.
    ///
    /// Either way a label here could only ever have been ignored, and an ignored label is exactly
    /// the silent-wrongness this module exists to prevent (see the module docs): `sub(b: 1, a: 10)`
    /// computing `1 - 10` is the same failure as `math.pow(exp: 3.0, base: 2.0)` computing `3²`.
    pub(crate) fn reject_unbound_labels(&mut self, args: &[CallArg], callee: &str) {
        if !CallArg::any_named(args) {
            return;
        }
        for arg in args.iter().filter(|a| a.name.is_some()) {
            self.error(
                DiagnosticCode::InvalidArgument,
                arg.span,
                format!("`{callee}` does not take named arguments"),
            )
            .help(
                "this callee declares no parameter names for a label to bind against; pass these \
                 arguments positionally",
            );
        }
    }

    /// Bind a **native module function's** labelled arguments, when its registry signature declares
    /// parameter names.
    ///
    /// Returns the list in parameter order with its labels consumed, or `None` to leave the call
    /// exactly as written — which happens for an unlabelled call (nothing to do) and for a
    /// signature that declares no names (the label cannot bind, and `check_args` refuses it).
    /// `arg_types` is permuted in place to stay parallel, as for a declared call.
    ///
    /// This is what puts a native call on the *same* binding path as a declared one, rather than a
    /// parallel implementation of it: same permutation, same supplied mask, same `arg_orders` entry
    /// for lowering, so `math.pow(exp: 3.0, base: 2.0)` reorders by the mechanism `sub(b:, a:)`
    /// already used.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_native_args(
        &mut self,
        module: &str,
        func: &str,
        args: &[CallArg],
        param_types: &[crate::Type],
        required: usize,
        arg_types: &mut [crate::Type],
        span: Span,
        call_span: Span,
    ) -> Option<Vec<CallArg>> {
        if !CallArg::any_named(args) {
            return None;
        }
        let sig = self.reg().find_function_sig(module, func)?;
        self.bind_sig_args(sig, args, param_types, required, arg_types, span, call_span)
    }

    /// The signature-keyed core of [`Self::bind_native_args`], for callers that already hold the
    /// [`ExtFn`] — a kernel/trait method, resolved from the receiver rather than a module path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_sig_args(
        &mut self,
        sig: &noeta_ext_abi::registry::ExtFn,
        args: &[CallArg],
        param_types: &[crate::Type],
        required: usize,
        arg_types: &mut [crate::Type],
        span: Span,
        call_span: Span,
    ) -> Option<Vec<CallArg>> {
        if !CallArg::any_named(args) || !sig.has_param_names() {
            return None;
        }
        let func = sig.name;
        let names: Vec<String> = sig.param_names.iter().map(|n| (*n).to_string()).collect();
        let bound = self.order_arguments(
            args,
            &names,
            param_types,
            required,
            func,
            arg_types,
            span,
            call_span,
        );
        // A label may REORDER a native call but never SKIP a parameter of one. A declared function
        // carries a supplied mask to its own prologue, which then runs the default for each
        // parameter the mask leaves clear; a native function has no prologue — its dispatch takes a
        // positional slice — so a hole would hand it a compacted list and shift every argument
        // after the gap onto the wrong parameter. Refuse the call instead, and drop the recorded
        // permutation so lowering does not act on a binding the callee cannot honour.
        if let Some(binding) = self.sites.arg_orders.get(&call_span)
            && let Some(hole) = binding.iter().position(Option::is_none)
            && binding[hole..].iter().any(Option::is_some)
        {
            let missing = names.get(hole).cloned().unwrap_or_default();
            self.sites.arg_orders.remove(&call_span);
            self.error(
                DiagnosticCode::InvalidArgument,
                span,
                format!("`{func}` cannot skip `{missing}` — a native function has no defaults to fall back on"),
            )
            .help("pass every parameter up to the last one you supply, or stop the argument list earlier");
            return Some(strip_labels(args));
        }
        // A binding that FAILED (an unknown or duplicated label) has already said so precisely.
        // Handing the labelled list back would let `check_args` add "does not take named
        // arguments" on top — which is both redundant and false, since this signature does. Return
        // the arguments label-free so the recovery path stays quiet about labels.
        Some(match bound {
            Some((ordered, _)) => ordered,
            None => strip_labels(args),
        })
    }

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
        // A pipeline's desugared call is marked at its span by `synth_piped`, which is what tells
        // binding that argument zero is the piped value and has no written position of its own.
        let piped = self.sites.piped_calls.contains(&call_span);
        let binding = self.bind_arguments(args, param_names, callee, piped)?;

        // A named argument that skips a defaulted parameter (`f(1, c: 9)`) leaves a *hole*, carried
        // to the callee as a **supplied mask** on the call: the callee still evaluates the default,
        // over its own upvalues, and the mask tells its prologue which ones to run rather than
        // inferring "the trailing remainder" from a count.
        //
        // The mask is one `u64`, and a method's is shifted up by one to make room for the receiver
        // at bit 0, so a *skipping* call can only reach the first `MASKED_PARAM_LIMIT` parameters —
        // one bound for both call kinds, since two that differ by kind is exactly how the tree-walker
        // (which never shifts) and the VM came to disagree about parameter 63 of a method.
        //
        // The bound is checked over the parameters the call **supplies**, not merely over where its
        // first hole falls. Lowering drops an out-of-range bit from the mask, and the argument then
        // lands on whichever parameter the shortened bit-count points at — so `f(1, z: 5)` over 66
        // parameters put nothing in `z` and nothing in the parameter it hit either (its default
        // overwrote the misplaced value), silently losing an explicitly written argument. The
        // first-hole test could not see that: the hole was parameter 1.
        //
        // Only skipping is limited. A call that fills a dense prefix carries no mask at all and is
        // unaffected at any arity, and so is a pure reordering.
        let last_supplied = binding.iter().rposition(Option::is_some);
        let skips = binding
            .iter()
            .take(last_supplied.map_or(0, |p| p + 1))
            .any(Option::is_none);
        if skips
            && let Some(p) = last_supplied.filter(|p| *p >= noeta_ast::reflect::MASKED_PARAM_LIMIT)
        {
            self.error(
                DiagnosticCode::InvalidArgument,
                span,
                format!(
                    "`{callee}` skips a parameter, so it cannot also name `{}` — only the first {} parameters can be named by a skipping call",
                    param_names[p],
                    noeta_ast::reflect::MASKED_PARAM_LIMIT
                ),
            )
            .help("pass the skipped parameter explicitly, or move this one earlier in the list");
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
        // Binding has CONSUMED the labels: the list that comes out is in parameter order, so a
        // label on it would only be a second, redundant statement of where its value already sits.
        // Clearing them is what lets [`Checker::check_args`] treat any *surviving* label as proof
        // that nothing bound it — see the rejection there.
        let ordered: Vec<CallArg> = order
            .iter()
            .map(|&i| CallArg {
                name: None,
                ..args[i].clone()
            })
            .collect();
        Some((ordered, supplied_params))
    }
}
