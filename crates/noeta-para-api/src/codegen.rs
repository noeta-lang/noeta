//! Turning a parsed [`Spec`] into the Noeta source `@openapi` splices into a declaration.
//!
//! The output is **source, not AST**, which is the expansion seam's design: it goes through the
//! real grammar, so a generator bug earns an ordinary parse or type error instead of corrupting a
//! tree, and `noeta expand` can print exactly what the compiler saw.
//!
//! ## The shape, and why
//!
//! Required parameters are **typed and positional**; optional query parameters arrive in a trailing
//! `query: Map<string, string> = {}`. The obvious alternative — one `?T` parameter per optional
//! query key — was tried and rejected: Noeta does not coerce `3` to `?int`, so every call would
//! read `list_pets(some(10))`. A map costs the optional keys their static types (they are listed in
//! the generated doc comment instead) and buys a call that looks like the call you meant.
//!
//! Everything is routed through `self.api.request(...)`, never `client.send` — the whole point of
//! `para/api` is that a generated call goes through the middleware chain like any other, so
//! `Mock`, `Cache` and `Logging` apply to generated clients for free.

use crate::spec::{In, Operation, Spec};

/// Names the generated members already own, which no operation may take.
///
/// A spec is free to contain an operation called `new`; the generated constructor is not
/// negotiable, so the operation is the one that moves.
const RESERVED_METHODS: [&str; 3] = ["new", "base_url", "api"];

/// Identifiers the generated *signatures* already use. A path parameter called `query` would
/// otherwise shadow the query map and silently drop every optional parameter.
const RESERVED_PARAMS: [&str; 4] = ["query", "body", "api", "self"];

/// Generate the whole member list for `target`.
pub fn client(target: &str, spec: &Spec) -> String {
    let mut out = String::new();

    out.push_str(
        "// The middleware chain every generated call goes through. Generated rather than required\n\
         // of you, so the constructor below can be generated too.\n\
         api: Api\n\n",
    );
    out.push_str(&format!(
        "fn new(api: Api): {target} {{\n    return {target} {{ api: api }}\n}}\n\n"
    ));

    if let Some(base) = &spec.base_url {
        out.push_str(&format!(
            "// The first server the spec names, so a client can be built without copying the URL.\n\
             fn base_url(): string {{\n    return \"{}\"\n}}\n\n",
            escape(base)
        ));
    }

    // Two operations can legitimately want the same method name — a spec with duplicate
    // `operationId`s, or two paths whose derived names collide. Later ones are suffixed rather than
    // silently overwriting, because losing an operation is the failure nobody notices.
    let mut taken: Vec<String> = RESERVED_METHODS.iter().map(|s| s.to_string()).collect();
    for op in &spec.operations {
        let name = unique(&op.name, &mut taken);
        out.push_str(&method(&name, op));
        out.push('\n');
    }
    out
}

/// One operation as one method.
fn method(name: &str, op: &Operation) -> String {
    let mut out = String::new();

    if let Some(doc) = &op.doc {
        // A summary may be multi-line; a comment may not be, so it is folded rather than truncated.
        for line in doc.lines().filter(|l| !l.trim().is_empty()) {
            out.push_str(&format!("// {}\n", line.trim()));
        }
    }
    out.push_str(&format!("// {} {}\n", op.method.to_uppercase(), op.path));

    // The optional keys lose their static types to the query map, so the doc comment is where a
    // caller finds out they exist at all. Dropping this would make them undiscoverable.
    let optional: Vec<&crate::spec::Param> = op.params.iter().filter(|p| !p.required).collect();
    if !optional.is_empty() {
        let listed: Vec<String> = optional
            .iter()
            .map(|p| format!("{} ({})", p.name, p.ty))
            .collect();
        out.push_str(&format!(
            "// Optional query parameters, by key: {}\n",
            listed.join(", ")
        ));
    }

    // --- signature ---
    let mut params = Vec::new();
    let mut names: Vec<String> = RESERVED_PARAMS.iter().map(|s| s.to_string()).collect();
    let mut bound = Vec::new();
    for p in op.params.iter().filter(|p| p.required) {
        let ident = unique(&crate::spec::snake_case(&p.name), &mut names);
        params.push(format!("{ident}: {}", p.ty));
        bound.push((p, ident));
    }
    if op.has_body {
        params.push("body: string = \"\"".to_string());
    }
    params.push("query: Map<string, string> = {}".to_string());
    out.push_str(&format!(
        "fn {name}({}): Result<Response, HttpError> {{\n",
        params.join(", ")
    ));

    // --- path ---
    // The template's `{placeholder}`s are substituted rather than interpolated, so a parameter
    // whose value happens to look like a placeholder cannot rewrite the path.
    let mut path = format!("\"{}\"", escape(&op.path));
    for (p, ident) in bound.iter().filter(|(p, _)| p.loc == In::Path) {
        // `Api.encode`, not a bare `url.encode`: generated members land in the user's file, so
        // anything they name has to be a name that file can resolve. `Api` is one the author
        // already imported to write the decorated struct at all, and the linker qualifies it here
        // exactly as it does in hand-written code — including when the consumer renamed the
        // dependency, which a hard-coded `para.api.Api` would get wrong.
        //
        // Called on the type rather than through `self.api` because it takes no receiver: Noeta
        // infers a method static when its body never touches `self`, and encoding needs no state.
        path.push_str(&format!(
            ".replace(\"{{{}}}\", Api.encode(\"${{{ident}}}\"))",
            escape(&p.name)
        ));
    }
    out.push_str(&format!("    path = {path}\n"));

    // --- query ---
    // Required query parameters join the caller's map rather than being concatenated separately, so
    // there is exactly one place that builds a query string and one place that encodes it.
    let mut map = "query".to_string();
    for (p, ident) in bound.iter().filter(|(p, _)| p.loc == In::Query) {
        map = format!("{map}.set(\"{}\", \"${{{ident}}}\")", escape(&p.name));
    }

    let body = if op.has_body { ", body" } else { "" };
    out.push_str(&format!(
        "    return self.api.request(\"{}\", path ~ Api.query_string({map}){body})\n}}\n",
        op.method
    ));
    out
}

/// Make `name` unique against everything already emitted, recording it.
fn unique(name: &str, taken: &mut Vec<String>) -> String {
    if !taken.iter().any(|t| t == name) {
        taken.push(name.to_string());
        return name.to_string();
    }
    // Start at 2: the first collision produces `get_pet_2`, which reads as "the second get_pet"
    // rather than implying there was a `get_pet_1`.
    let mut n = 2;
    loop {
        let candidate = format!("{name}_{n}");
        if !taken.iter().any(|t| t == &candidate) {
            taken.push(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

/// Escape a spec string for a Noeta string literal.
///
/// A spec is remote input: a path or summary containing a quote, a backslash, or `${` would
/// otherwise close the literal or open an interpolation, turning a document into code. That is the
/// injection this generator has to be immune to.
fn escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            // `$` only opens an interpolation before `{`, but escaping it unconditionally is one
            // rule instead of two and cannot be got wrong.
            '$' => out.push_str("\\$"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;

    fn generate(json: &str) -> String {
        client("PetStore", &spec::parse(json).expect("spec parses"))
    }

    #[test]
    fn the_field_and_constructor_are_generated_not_demanded() {
        let out = generate(r#"{"paths":{"/pets":{"get":{"operationId":"listPets"}}}}"#);
        assert!(out.contains("api: Api"), "{out}");
        assert!(out.contains("fn new(api: Api): PetStore"), "{out}");
        assert!(out.contains("return PetStore { api: api }"), "{out}");
    }

    #[test]
    fn a_path_parameter_is_substituted_and_encoded() {
        let out = generate(
            r#"{"paths":{"/pets/{petId}":{"get":{"operationId":"showPetById","parameters":[
               {"name":"petId","in":"path","schema":{"type":"string"}}]}}}}"#,
        );
        assert!(
            out.contains(r#"path = "/pets/{petId}".replace("{petId}", Api.encode("${pet_id}"))"#),
            "{out}"
        );
    }

    #[test]
    fn a_required_query_parameter_joins_the_callers_map() {
        let out = generate(
            r#"{"paths":{"/search":{"get":{"operationId":"search","parameters":[
               {"name":"q","in":"query","required":true,"schema":{"type":"string"}}]}}}}"#,
        );
        assert!(
            out.contains("fn search(q: string, query: Map<string, string> = {})"),
            "{out}"
        );
        assert!(
            out.contains(r#"Api.query_string(query.set("q", "${q}"))"#),
            "{out}"
        );
    }

    #[test]
    fn optional_parameters_are_documented_since_the_map_erases_their_types() {
        let out = generate(
            r#"{"paths":{"/pets":{"get":{"operationId":"listPets","parameters":[
               {"name":"limit","in":"query","schema":{"type":"integer"}}]}}}}"#,
        );
        assert!(
            out.contains("// Optional query parameters, by key: limit (int)"),
            "{out}"
        );
    }

    #[test]
    fn a_body_operation_takes_a_body_and_a_bodyless_one_does_not() {
        let with = generate(
            r#"{"paths":{"/pets":{"post":{"operationId":"createPet","requestBody":{}}}}}"#,
        );
        assert!(
            with.contains(r#"fn create_pet(body: string = "", query"#),
            "{with}"
        );
        assert!(with.contains(", body)"), "{with}");

        let without = generate(r#"{"paths":{"/pets":{"get":{"operationId":"listPets"}}}}"#);
        assert!(!without.contains("body"), "{without}");
    }

    #[test]
    fn every_call_goes_through_the_middleware_chain() {
        // Not `client.send`: a generated client that bypassed the chain would silently escape
        // `Mock` in tests and `Logging` in production, which is the whole value of `para/api`.
        let out = generate(r#"{"paths":{"/pets":{"get":{"operationId":"listPets"}}}}"#);
        assert!(out.contains("self.api.request("), "{out}");
        assert!(!out.contains("client.send"), "{out}");
    }

    #[test]
    fn a_spec_cannot_inject_code_through_a_string_it_controls() {
        // Paths and summaries are remote input. A quote or a `${` in either must stay data.
        let out = generate(
            r#"{"paths":{"/a\"b":{"get":{"operationId":"x","summary":"hi"}}},
               "servers":[{"url":"http://h/${evil}"}]}"#,
        );
        assert!(out.contains(r#"path = "/a\"b""#), "{out}");
        assert!(out.contains(r"http://h/\${evil}"), "{out}");
    }

    #[test]
    fn an_operation_may_not_take_a_generated_members_name() {
        // A spec with an operation called `new` must not displace the constructor.
        let out = generate(
            r#"{"paths":{"/a":{"get":{"operationId":"new"}},"/b":{"get":{"operationId":"new"}}}}"#,
        );
        assert!(out.contains("fn new(api: Api): PetStore"), "{out}");
        assert!(out.contains("fn new_2("), "{out}");
        assert!(out.contains("fn new_3("), "{out}");
    }

    #[test]
    fn a_parameter_may_not_shadow_the_query_map() {
        // A path parameter literally named `query` would otherwise bind over the trailing map and
        // silently drop every optional parameter.
        let out = generate(
            r#"{"paths":{"/x/{query}":{"get":{"operationId":"x","parameters":[
               {"name":"query","in":"path","schema":{"type":"string"}}]}}}}"#,
        );
        assert!(
            out.contains("fn x(query_2: string, query: Map<string, string> = {})"),
            "{out}"
        );
        assert!(out.contains(r#"Api.encode("${query_2}")"#), "{out}");
    }
}
