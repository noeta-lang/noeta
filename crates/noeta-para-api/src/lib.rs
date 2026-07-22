//! The `para/api` package's native half: the **`@openapi` directive**, and the percent-encoder its
//! generated code needs.
//!
//! `para/api` is otherwise pure Noeta — the middleware onion, the standard layers and the
//! pagination strategies are all ordinary Noeta values, deliberately, because composition invokes
//! user code and native code holding user closures was tried once and leaked (see
//! `plans/http/para-api-handoff.md`). Native code enters here for the one thing Noeta cannot do:
//! **read a file at compile time and turn it into declarations.**
//!
//! ```noeta ignore
//! use para.api.Api
//! use std.http.{Response, HttpError}
//!
//! @openapi("petstore.json")
//! struct PetStore {}
//!
//! store = PetStore.new(Api.new(client.new(PetStore.base_url())))
//! resp = store.show_pet_by_id("7")?
//! ```
//!
//! The struct is written empty: the directive generates the `api` field, a `new`, a `base_url()`,
//! and one method per operation in the spec. Run `noeta expand` to see exactly what — that output
//! is the real parsed source, so a spec change is a reviewable diff.
//!
//! ## The ceiling, stated plainly
//!
//! An expansion contributes **members of the declaration it decorates**, and nothing else. So the
//! generated methods answer `Response` and the caller decodes with `resp.json::<T>()`; this
//! directive cannot generate the `Pet` struct itself, because that would be a sibling declaration.
//! That is a property of the expansion seam, not a gap in this crate — `ExtDirective::sites` is the
//! whole answer to output scope.

use noeta_ext_abi::registry::{
    DirectiveCtx, Expansion, ExpansionError, ExtDirective, ExtFn, ExtModule, Extension, NativeOut,
    NativeValue, RetTy, SigType, TierSite,
};
use noeta_ext_abi::{Host, StdError};

pub mod codegen;
pub mod spec;

/// `@openapi("spec.json")` — generate a client from an OpenAPI document.
///
/// `sites` is `Type` alone: the members this generates are methods with a `self`, so the directive
/// belongs on a `struct`/`class`, never on a function or a trait. `max_args` is 1 and there are no
/// named keys, because everything else the generator needs it reads out of the document — a
/// directive with knobs is a directive whose output you cannot predict from the spec.
const OPENAPI: ExtDirective = ExtDirective {
    name: "openapi",
    sites: &[TierSite::Type],
    max_args: Some(1),
    named_keys: &[],
    detail: "@openapi(\"spec.json\") — generate a client from an OpenAPI document",
    doc: "Generates one method per operation in the named OpenAPI document, plus the `api` field \
          they call through, a `new(api)` constructor, and `base_url()` from the spec's first \
          server. The path is resolved against the file the directive is written in. Only JSON \
          specs are read — convert a YAML spec first. Generated methods answer \
          `Result<Response, HttpError>`; decode with `resp.json::<T>()`. Run `noeta expand` to see \
          what it produced.",
    params: &["spec"],
    expand: Some(expand_openapi),
};

/// Read the document named by the directive's one argument and generate the client.
///
/// Every path is resolved against [`DirectiveCtx::source_dir`] — the directory of the file the
/// directive was written in — so a spec sits next to the code that uses it and moving the pair
/// keeps working, which resolving against the process's working directory would not.
fn expand_openapi(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    // `max_args: Some(1)` is checked before a hook runs, so at most one argument arrives — but
    // *zero* is also legal under that contract, and `args[0]` would panic. A hook must still handle
    // the shapes its declaration permits; it is only relieved of the ones it forbids.
    let Some(arg) = ctx.args.first() else {
        // No path was named, so nothing was read — a bare message with empty reads.
        return Err(
            "needs the path to an OpenAPI document, as in `@openapi(\"petstore.json\")`".into(),
        );
    };

    let path = std::path::Path::new(&ctx.source_dir).join(arg);
    let display = path.display().to_string();

    // Named before it is opened, so it is reported whether or not the read succeeds — and reported
    // on the *error* paths too, which is the whole point. This is the incrementality contract: a
    // spec that is missing today and written tomorrow has to re-trigger this expansion, and it can
    // only do that if the compiler was told the path was consulted. `failed` bundles that read set
    // onto every failure below.
    let reads = vec![display.clone()];
    let failed = |message: String| ExpansionError {
        message,
        reads: reads.clone(),
    };

    if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml" | "yml")
    ) {
        return Err(failed(format!(
            "`{arg}` is YAML, and only JSON specs are read — convert it first \
             (any OpenAPI tool will, and both spellings are the same document)"
        )));
    }

    let text = std::fs::read_to_string(&path)
        .map_err(|e| failed(format!("could not read `{display}`: {e}")))?;
    let spec = spec::parse(&text).map_err(|e| failed(format!("`{arg}`: {e}")))?;

    Ok(Expansion {
        source: codegen::client(&ctx.target, &spec),
        reads,
    })
}

/// `para.url` — the percent-encoder generated query strings are built with.
///
/// It is native because correct percent-encoding is byte-wise over UTF-8, which Noeta's string
/// surface does not reach; and it lives in `para/api` rather than `std.http` because std owns
/// transport and this is the composition layer's need. If std ever grows one, this becomes a
/// forward.
///
/// It is `para.url` and **not** `para.api.url` because module nesting only runs one way: a native
/// module may be the parent of Noeta modules (as `para.db` is of `para.db.query`), but a native
/// module cannot hang beneath a Noeta namespace — `para.api` is `api.noe`, so `para.api.url`
/// resolves as an export of that file and is not found. Nothing outside the package should need
/// this name anyway: `Api` re-exposes both entry points as methods precisely so generated code and
/// user code alike reach them through the `Api` they already hold.
const URL_FNS: &[ExtFn] = &[ExtFn {
    name: "encode",
    params: &[SigType::String],
    ret: RetTy::Concrete(SigType::String),
}];

fn url_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "encode" => {
            let NativeValue::Str(s) = &args[0] else {
                return Ok(NativeOut::Str(String::new()));
            };
            Ok(NativeOut::Str(percent_encode(s)))
        }
        _ => Err(noeta_ext_abi::no_function_error("url", func)),
    }
}

/// RFC 3986 percent-encoding, with the unreserved set left alone.
///
/// Encoding is over **bytes**, not characters: a multi-byte character becomes one `%XX` per UTF-8
/// byte, which is what a server decodes back. Encoding per character would produce something no
/// server accepts.
fn percent_encode(value: &str) -> String {
    const UNRESERVED: &[u8] = b"-_.~";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `para/api` extension: one directive, and the one module its output calls into.
#[derive(Debug, Clone, Copy)]
pub struct ParaApiExtension;

impl Extension for ParaApiExtension {
    fn name(&self) -> &'static str {
        "para.api"
    }

    fn root(&self) -> &'static str {
        "para"
    }

    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "url",
            functions: URL_FNS,
            dispatch: url_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }

    fn directives(&self) -> &'static [ExtDirective] {
        &[OPENAPI]
    }
}

/// The fixed native-extension export convention (package-manager Phase 3): the composed toolchain
/// aggregates each native dependency's slice and installs the union into the runtime registry.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ParaApiExtension];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_leaves_the_unreserved_set_alone() {
        assert_eq!(percent_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
    }

    #[test]
    fn percent_encoding_covers_every_character_that_breaks_a_query_string() {
        // These are the ones that turn one parameter into two, or a query into a fragment — the
        // whole reason this exists rather than raw concatenation.
        assert_eq!(percent_encode("a&b=c?d#e"), "a%26b%3Dc%3Fd%23e");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("100%"), "100%25");
    }

    #[test]
    fn percent_encoding_is_over_bytes_not_characters() {
        // One `%XX` per UTF-8 byte. Encoding per `char` would emit something no server decodes.
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("日"), "%E6%97%A5");
    }

    #[test]
    fn the_directive_declares_only_what_it_honours() {
        // `max_args`/`named_keys` are enforced by the compiler before the hook runs, so an
        // over-generous declaration would hand the hook shapes it does not handle.
        assert_eq!(OPENAPI.max_args, Some(1));
        assert!(OPENAPI.named_keys.is_empty());
        assert_eq!(OPENAPI.sites, &[TierSite::Type]);
        assert!(OPENAPI.expand.is_some());
    }

    #[test]
    fn a_missing_argument_is_answered_rather_than_panicking() {
        // `max_args: Some(1)` permits zero, so the hook must survive it — the contract removes the
        // shapes it forbade, not the ones it allowed.
        let ctx = DirectiveCtx {
            args: Vec::new(),
            named: Vec::new(),
            target: "PetStore".to_string(),
            site: TierSite::Type,
            source_dir: String::new(),
        };
        let error = expand_openapi(&ctx).expect_err("no argument cannot expand");
        assert!(
            error.message.contains("needs the path"),
            "{}",
            error.message
        );
        // No path was named, so nothing was read.
        assert!(error.reads.is_empty(), "{:?}", error.reads);
    }

    #[test]
    fn a_yaml_spec_is_refused_by_name_rather_than_parsed_hopefully() {
        let ctx = DirectiveCtx {
            args: vec!["petstore.yaml".to_string()],
            named: Vec::new(),
            target: "PetStore".to_string(),
            site: TierSite::Type,
            source_dir: "/proj".to_string(),
        };
        let error = expand_openapi(&ctx).expect_err("yaml is not read");
        assert!(
            error.message.contains("convert it first"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_failure_reports_the_spec_it_tried_so_creating_it_re_runs() {
        // The reads-on-error contract: a missing spec still reports the path it looked for, because
        // that path *appearing* is exactly what must re-trigger the expansion. A `Result<_, String>`
        // could not carry this — the reads lived only in the `Ok`.
        let ctx = DirectiveCtx {
            args: vec!["does-not-exist.json".to_string()],
            named: Vec::new(),
            target: "PetStore".to_string(),
            site: TierSite::Type,
            source_dir: "/proj/api".to_string(),
        };
        let error = expand_openapi(&ctx).expect_err("a missing spec cannot expand");
        assert!(
            error.message.contains("could not read"),
            "{}",
            error.message
        );
        assert_eq!(
            error.reads,
            vec!["/proj/api/does-not-exist.json".to_string()],
            "the missing spec must be reported so its later appearance re-runs the hook"
        );
    }
}
