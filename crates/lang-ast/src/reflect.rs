//! A pure, deterministic projection of the AST into **reflection data** — the attribute manifest
//! plus a registry of every declared type's reflectable shape. Both backends build it from the same
//! [`Program`] via [`build`], so runtime reflection (attribute-system pass 2) is identical across
//! the tree-walker and the VM **by construction** — there is no second walk to drift from the first.
//! It carries no codegen or runtime meaning of its own; it is a read-only view of the program.

use crate::{AttrArg, AttrValue, Attribute, Program, Stmt};

/// Everything reflection needs about a program, derived purely from its AST.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReflectionInfo {
    /// Every `#[...]` data attribute, in source order, each keyed by the declaration it annotates.
    pub manifest: Vec<AttributeRecord>,
    /// Every declared record/class/enum, in source order.
    pub types: Vec<TypeInfo>,
}

impl ReflectionInfo {
    /// The data attributes attached to `target`, in source order — the manifest query tooling and
    /// `attributes_of` use to discover, e.g., every type tagged `#[Entity]`.
    pub fn attributes_for<'a>(
        &'a self,
        target: &'a str,
    ) -> impl Iterator<Item = &'a AttributeRecord> {
        self.manifest.iter().filter(move |a| a.target == target)
    }

    /// The reflectable shape of a declared type by name.
    pub fn type_named(&self, name: &str) -> Option<&TypeInfo> {
        self.types.iter().find(|t| t.name == name)
    }
}

/// One `#[Name(args)]` attached to a declaration. Semantically a record instance attached as
/// metadata; the runtime materializes it from the stored args (pass 2).
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeRecord {
    /// The annotated declaration's name (a type name today; pass 2 extends attributes to methods).
    pub target: String,
    /// The attribute's name (e.g. `Route`).
    pub name: String,
    /// The attribute's literal arguments (positional + named), straight from the AST.
    pub args: Vec<AttrArg>,
}

/// The kind of a declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Record,
    Class,
    Enum,
}

/// A declared type's reflectable shape: name, kind, and member names (declaration order). Field and
/// variant *types* are deliberately absent — they are erased at runtime, and reflection over a value
/// recovers names, not the static field types (which are a compile-time `type_of` concern).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    /// Field names in declaration order (records/classes; empty for enums).
    pub fields: Vec<String>,
    /// Variants in declaration order (enums; empty otherwise).
    pub variants: Vec<VariantInfo>,
}

/// An enum variant's reflectable shape.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantInfo {
    pub name: String,
    pub fields: Vec<String>,
}

/// Build the reflection info for a program. **Pure and deterministic**: the same AST always yields
/// the same [`ReflectionInfo`], so both backends calling this on the same [`Program`] agree without
/// any cross-backend coordination — the property the differential oracle depends on.
pub fn build(program: &Program) -> ReflectionInfo {
    let mut manifest = Vec::new();
    let mut types = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Record(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Record,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    variants: Vec::new(),
                });
            }
            Stmt::Class(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Class,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    variants: Vec::new(),
                });
            }
            Stmt::Enum(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Enum,
                    fields: Vec::new(),
                    variants: decl
                        .variants
                        .iter()
                        .map(|v| VariantInfo {
                            name: v.name.clone(),
                            fields: v.fields.iter().map(|f| f.name.clone()).collect(),
                        })
                        .collect(),
                });
            }
            _ => {}
        }
    }
    ReflectionInfo { manifest, types }
}

/// Resolve an attribute's arguments to its record's field values, in declaration order — the shared
/// mapping both backends use to materialize the attribute instance, so they build identical values.
/// Positional arguments fill fields left to right; named arguments bind by field name. The use-site
/// construction check (E0009/E0007/E0005) guarantees a well-formed program supplies exactly one
/// value per field, so a runnable program never leaves a field unresolved.
pub fn materialize_args(attr: &AttributeRecord, fields: &[String]) -> Vec<AttrValue> {
    let mut values: Vec<Option<AttrValue>> = vec![None; fields.len()];
    let mut next_positional = 0usize;
    for arg in &attr.args {
        let idx = match &arg.name {
            Some(fname) => fields.iter().position(|f| f == fname),
            None => {
                let i = next_positional;
                next_positional += 1;
                Some(i)
            }
        };
        if let Some(i) = idx
            && i < values.len()
        {
            values[i] = Some(arg.value.clone());
        }
    }
    // A field with no supplied value cannot occur in a runnable program (the construction check
    // rejects it); the `unit` fallback is unreachable defensive code.
    values
        .into_iter()
        .map(|v| v.unwrap_or(AttrValue::Bool(false)))
        .collect()
}

fn push_attrs(manifest: &mut Vec<AttributeRecord>, target: &str, attrs: &[Attribute]) {
    for attr in attrs {
        manifest.push(AttributeRecord {
            target: target.to_string(),
            name: attr.name.clone(),
            args: attr.args.clone(),
        });
    }
}
