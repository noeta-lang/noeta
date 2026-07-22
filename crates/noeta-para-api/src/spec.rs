//! The slice of an OpenAPI document a client generator actually needs.
//!
//! An OpenAPI document describes far more than a client: response schemas, examples, security
//! schemes, vendor extensions. This module reads only what determines a **call** — the servers, and
//! for each operation its method, path, parameters and whether it carries a body — and ignores the
//! rest rather than modelling it. That is why the document is walked as an untyped
//! [`serde_json::Value`] instead of deserialized into derived structs: a strict deserialize would
//! reject real documents over keys the generator never reads, which is a worse failure than
//! silently not using them.
//!
//! ## What is deliberately not here
//!
//! **Response schemas.** The generated methods answer `Response`, and decoding is the caller's
//! `resp.json::<T>()`. Generating result *types* would mean generating sibling declarations, and
//! expansion cannot: a hook contributes members of the declaration it decorates and nothing else
//! (`ExtDirective::sites` is the whole answer to output scope). That is a real ceiling, not an
//! omission — see the crate docs.
//!
//! **YAML.** Only JSON is read. Both spellings are legal OpenAPI and every toolchain can emit
//! either, so this costs a conversion rather than a capability, and it keeps a YAML parser out of
//! the toolchain's supply chain. A `.yaml` argument is rejected by name with that advice, never
//! parsed hopefully and then failed obscurely.

use serde_json::Value;

/// Where a parameter is carried. Only the two that shape a URL are modelled — `header` and `cookie`
/// parameters are ignored, because middleware is the right place to set those and a generated
/// signature that demanded them would fight the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum In {
    Path,
    Query,
}

/// One parameter of one operation.
#[derive(Debug, Clone)]
pub struct Param {
    /// The wire name — the query key, or the `{placeholder}` in the path template.
    pub name: String,
    pub loc: In,
    /// A path parameter is required whatever the document says (the URL cannot be built without
    /// it); a query parameter is required only if it says so.
    pub required: bool,
    /// The Noeta type this parameter is generated as, already mapped from the JSON Schema type.
    pub ty: &'static str,
}

/// One generated method's worth of the document.
#[derive(Debug, Clone)]
pub struct Operation {
    /// Lower-case HTTP verb, as `Api.request` wants it.
    pub method: String,
    /// The path template, `{placeholder}`s intact — substitution is generated, not done here.
    pub path: String,
    /// The generated method name.
    pub name: String,
    /// Path parameters first, then required query parameters, then optional ones. Generated
    /// signatures follow this order because Noeta defaults must come last, and a stable order means
    /// a spec edit that adds an optional parameter does not renumber the existing ones.
    pub params: Vec<Param>,
    /// Whether the operation declares a request body, so the generator knows to take one.
    pub has_body: bool,
    /// The operation's `summary`/`description`, if any, for the generated doc comment.
    pub doc: Option<String>,
}

/// Everything the generator reads out of one document.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The first `servers[].url`, if the document names one. Generated as `base_url()` so a caller
    /// can build a client against the API the spec describes without copying the URL by hand.
    pub base_url: Option<String>,
    pub operations: Vec<Operation>,
}

/// The HTTP verbs a path item may carry. `parameters`, `summary`, `$ref` and the rest of a path
/// item's keys are not operations, so the verbs are enumerated rather than inferred from "every key
/// that maps to an object".
const METHODS: [&str; 7] = ["get", "put", "post", "delete", "options", "head", "patch"];

/// Parse a document into the slice above.
///
/// Every error is phrased for the person who wrote `@openapi(...)`, not for someone debugging this
/// crate: they can see their spec and cannot see the generator, so "no `paths` object" beats a
/// serde type error naming an internal path.
pub fn parse(text: &str) -> Result<Spec, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| {
        format!(
            "the spec is not valid JSON: {e} (only JSON specs are read — convert a YAML spec first)"
        )
    })?;

    let base_url = doc
        .get("servers")
        .and_then(Value::as_array)
        .and_then(|s| s.first())
        .and_then(|s| s.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let paths = doc
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("the spec has no `paths` object")?;

    let mut operations = Vec::new();
    // `paths` is a JSON object, and `serde_json`'s default map preserves insertion order, so
    // generated methods come out in document order — a spec edit produces a reviewable diff rather
    // than a reshuffle.
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        // Parameters declared on the path item are shared by every operation under it, so they are
        // read once and prepended to each. Missing this is the classic OpenAPI generator bug: the
        // `{id}` declared once at the path level vanishes from every method that needs it.
        let shared = params_of(item.get("parameters"), &doc, path)?;
        for method in METHODS {
            let Some(op) = item.get(method).and_then(Value::as_object) else {
                continue;
            };
            let mut params = shared.clone();
            params.extend(params_of(op.get("parameters"), &doc, path)?);
            operations.push(operation(method, path, op, params)?);
        }
    }

    if operations.is_empty() {
        return Err("the spec declares no operations".to_string());
    }
    Ok(Spec {
        base_url,
        operations,
    })
}

/// Build one operation, ordering and de-duplicating its parameters.
fn operation(
    method: &str,
    path: &str,
    op: &serde_json::Map<String, Value>,
    mut params: Vec<Param>,
) -> Result<Operation, String> {
    // An operation-level parameter overrides a path-level one of the same name and location, which
    // is what the specification says and what a reader expects: the more specific declaration wins.
    // The extend above appended the operation's, so keeping the LAST of each duplicate is the rule.
    let mut seen = Vec::new();
    params.reverse();
    params.retain(|p| {
        let key = (p.name.clone(), p.loc);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    params.reverse();

    // Required before optional, because Noeta requires defaulted parameters to come last. Within
    // each group the document's order is kept — `sort_by_key` is stable.
    params.sort_by_key(|p| !p.required);

    let name = match op.get("operationId").and_then(Value::as_str) {
        Some(id) => snake_case(id),
        // A document without `operationId` is legal, and plenty of hand-written ones omit it, so a
        // derived name is better than a refusal. It is derived from the method and path, which are
        // the only things guaranteed unique per operation.
        None => derived_name(method, path),
    };

    Ok(Operation {
        method: method.to_string(),
        path: path.to_string(),
        name,
        params,
        has_body: op.contains_key("requestBody"),
        doc: op
            .get("summary")
            .or_else(|| op.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Read a `parameters` array, resolving each entry's `$ref` if it has one.
fn params_of(value: Option<&Value>, doc: &Value, path: &str) -> Result<Vec<Param>, String> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in list {
        let entry = resolve(entry, doc)?;
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let loc = match entry.get("in").and_then(Value::as_str) {
            Some("path") => In::Path,
            Some("query") => In::Query,
            // `header` and `cookie` parameters are intentionally dropped — see `In`.
            _ => continue,
        };
        // A path parameter is required by construction: the template cannot be filled without it,
        // so a document that marks one optional is describing something unbuildable. Trusting the
        // path over the flag turns a spec bug into working code rather than a broken signature.
        let required = loc == In::Path
            || entry
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if loc == In::Path && !path.contains(&format!("{{{name}}}")) {
            return Err(format!(
                "`{path}` declares a path parameter `{name}` that the path template does not contain"
            ));
        }
        out.push(Param {
            name: name.to_string(),
            loc,
            required,
            ty: noeta_type(entry.get("schema")),
        });
    }
    Ok(out)
}

/// Follow a local `$ref`, or return the value unchanged.
///
/// Only in-document refs (`#/components/parameters/Id`) are followed. An external one names a file
/// this generator would have to read — and therefore report in `Expansion::reads` — so it is
/// refused explicitly rather than silently ignored, which would produce a method quietly missing a
/// parameter.
fn resolve<'a>(value: &'a Value, doc: &'a Value) -> Result<&'a Value, String> {
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
        return Ok(value);
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        return Err(format!(
            "`{reference}` refers to another file, which is not supported yet — inline it, or bundle the spec first"
        ));
    };
    doc.pointer(pointer)
        .ok_or_else(|| format!("`{reference}` does not resolve inside the spec"))
}

/// Map a JSON Schema type to the Noeta type a parameter is generated as.
///
/// Anything not scalar becomes `string`: a query parameter's serialization for arrays and objects
/// is governed by `style`/`explode`, which this generator does not implement, and emitting
/// `List<int>` while serializing it wrongly would be worse than handing the caller the string and
/// letting them encode it.
fn noeta_type(schema: Option<&Value>) -> &'static str {
    match schema.and_then(|s| s.get("type")).and_then(Value::as_str) {
        Some("integer") => "int",
        Some("number") => "float",
        Some("boolean") => "bool",
        _ => "string",
    }
}

/// `listPets` / `list-pets` / `List Pets` → `list_pets`.
///
/// `operationId` has no required spelling, so every convention in the wild has to land on Noeta's.
pub fn snake_case(id: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in id.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else if !out.ends_with('_') && !out.is_empty() {
            // Any separator — `-`, ` `, `.`, `/` — collapses to one underscore.
            out.push('_');
            prev_lower = false;
        }
    }
    let out = out.trim_matches('_').to_string();
    // A name has to be an identifier: an `operationId` that was all punctuation, or one starting
    // with a digit, would otherwise generate code that does not parse.
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        format!("op_{out}")
    } else {
        out
    }
}

/// The method name for an operation with no `operationId`: the verb, then the path with each
/// `{placeholder}` spelled `by_<name>` so two paths differing only in a parameter do not collide.
fn derived_name(method: &str, path: &str) -> String {
    let mut parts = vec![method.to_string()];
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(param) => parts.push(format!("by_{}", snake_case(param))),
            None => parts.push(snake_case(segment)),
        }
    }
    parts.join("_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_lands_every_convention_on_one() {
        assert_eq!(snake_case("listPets"), "list_pets");
        assert_eq!(snake_case("list-pets"), "list_pets");
        assert_eq!(snake_case("List Pets"), "list_pets");
        assert_eq!(snake_case("pets.list"), "pets_list");
        assert_eq!(snake_case("HTTPProxy"), "httpproxy");
        // Must still be an identifier afterwards, whatever went in.
        assert_eq!(snake_case("2fa"), "op_2fa");
        assert_eq!(snake_case("---"), "op_");
    }

    #[test]
    fn a_path_parameter_is_required_even_when_the_document_says_otherwise() {
        let spec = parse(
            r#"{"paths":{"/pets/{id}":{"get":{"operationId":"getPet","parameters":[
               {"name":"id","in":"path","required":false,"schema":{"type":"string"}}]}}}}"#,
        )
        .expect("spec parses");
        assert!(spec.operations[0].params[0].required);
    }

    #[test]
    fn a_path_level_parameter_reaches_every_operation_under_it() {
        let spec = parse(
            r#"{"paths":{"/pets/{id}":{
               "parameters":[{"name":"id","in":"path","schema":{"type":"string"}}],
               "get":{"operationId":"getPet"},"delete":{"operationId":"deletePet"}}}}"#,
        )
        .expect("spec parses");
        assert_eq!(spec.operations.len(), 2);
        for op in &spec.operations {
            assert_eq!(op.params.len(), 1, "{} lost the shared parameter", op.name);
        }
    }

    #[test]
    fn an_operation_parameter_overrides_the_path_level_one_it_shadows() {
        let spec = parse(
            r#"{"paths":{"/pets":{
               "parameters":[{"name":"limit","in":"query","schema":{"type":"string"}}],
               "get":{"operationId":"listPets","parameters":[
                 {"name":"limit","in":"query","required":true,"schema":{"type":"integer"}}]}}}}"#,
        )
        .expect("spec parses");
        let params = &spec.operations[0].params;
        assert_eq!(params.len(), 1, "the duplicate was not collapsed");
        assert_eq!(params[0].ty, "int", "the path-level declaration won");
        assert!(params[0].required);
    }

    #[test]
    fn required_parameters_are_ordered_before_optional_ones() {
        // Noeta requires defaulted parameters last, so this ordering is a correctness constraint on
        // the generated signature, not a stylistic one.
        let spec = parse(
            r#"{"paths":{"/pets/{id}":{"get":{"operationId":"getPet","parameters":[
               {"name":"verbose","in":"query","schema":{"type":"boolean"}},
               {"name":"id","in":"path","schema":{"type":"string"}}]}}}}"#,
        )
        .expect("spec parses");
        let names: Vec<&str> = spec.operations[0]
            .params
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["id", "verbose"]);
    }

    #[test]
    fn a_local_ref_resolves_and_an_external_one_is_refused() {
        // `r##"…"##`: the JSON pointers below contain `"#`, which would close an `r#"…"#` literal.
        let spec = parse(
            r##"{"paths":{"/pets/{id}":{"get":{"operationId":"getPet","parameters":[
               {"$ref":"#/components/parameters/Id"}]}}},
               "components":{"parameters":{"Id":{"name":"id","in":"path","schema":{"type":"integer"}}}}}"##,
        )
        .expect("spec parses");
        assert_eq!(spec.operations[0].params[0].ty, "int");

        let external = parse(
            r##"{"paths":{"/pets":{"get":{"operationId":"listPets","parameters":[
               {"$ref":"common.json#/Id"}]}}}}"##,
        )
        .expect_err("an external ref must be refused, never silently dropped");
        assert!(external.contains("another file"), "{external}");
    }

    #[test]
    fn a_missing_operation_id_derives_a_name_that_distinguishes_paths() {
        let spec = parse(
            r#"{"paths":{
               "/pets":{"get":{}},
               "/pets/{petId}":{"get":{"parameters":[{"name":"petId","in":"path"}]}}}}"#,
        )
        .expect("spec parses");
        let names: Vec<&str> = spec.operations.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["get_pets", "get_pets_by_pet_id"]);
    }

    #[test]
    fn a_document_that_cannot_generate_a_client_says_which_way_it_failed() {
        assert!(parse("not json").unwrap_err().contains("not valid JSON"));
        assert!(
            parse(r#"{"openapi":"3.0.0"}"#)
                .unwrap_err()
                .contains("`paths`")
        );
        assert!(
            parse(r#"{"paths":{}}"#)
                .unwrap_err()
                .contains("no operations")
        );
        // A path parameter the template cannot accept is a spec bug worth naming, because the
        // generated method would otherwise compile and then never substitute anything.
        let mismatch = parse(
            r#"{"paths":{"/pets":{"get":{"operationId":"listPets","parameters":[
               {"name":"id","in":"path"}]}}}}"#,
        )
        .expect_err("a path parameter with no placeholder must be refused");
        assert!(mismatch.contains("does not contain"), "{mismatch}");
    }
}
