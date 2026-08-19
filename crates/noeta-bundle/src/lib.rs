//! The `.noeb` bundle container (P-AOT L1.1): a versioned envelope around a serialized
//! [`noeta_bytecode::Module`], so a compiled program can be shipped and run **without its `.noe`
//! source**. This crate owns the artifact *format* — magic, versioning, and (later) the
//! obfuscation/encryption transforms — and is deliberately isolated from the core mid-end crates so
//! those never pull the container's future compression/crypto dependencies.
//!
//! ## Format
//!
//! ```text
//! ┌────────┬─────────┬───────┬────────┬──────────────┬──────────────────────┐
//! │ "NOEB" │ fmt_ver │ flags │ rt_len │ rt_ver bytes │ payload …            │
//! │ 4 B    │ u8      │ u8    │ u8     │ rt_len B     │ postcard(Module) …   │
//! └────────┴─────────┴───────┴────────┴──────────────┴──────────────────────┘
//! ```
//!
//! `fmt_ver` versions the *container* layout (this crate); `rt_ver` records the **runtime version**
//! that built the artifact. Payload compatibility is not self-describing (postcard is not), so v1's
//! policy is that **artifacts are pinned to the runtime that built them**: [`read`] rejects a
//! `rt_ver` mismatch with a clear error rather than risk decoding a stale layout.
//!
//! ## Obfuscation (P-AOT L1.4)
//!
//! The default payload is **obfuscated, not plaintext**: the postcard module is deflate-compressed
//! (a size win that also defeats `strings`/`grep`) and then XOR-scrambled with a fixed-seed
//! keystream, so `noeta dump`, a hex editor, and automated tooling all fail on the shipped file
//! (`FLAG_COMPRESSED` marks it). This is **obfuscation, honestly labeled — not security**: the
//! transform is fully reversible from this open-source runtime, and the module is recoverable from
//! process memory at run time. It raises the bar against casual inspection, nothing more. Access
//! control / encrypt-at-rest is deliberately **not** provided here — that is application *policy*,
//! the developer's to build on the crypto/network primitives the language ships (`plans/aot`).
//! `FLAG_ENCRYPTED` (bit 1) stays a reserved header bit for forward-compat; a reader rejects any
//! bundle that sets it.
//!
//! ## Stapled executables (P-AOT L2)
//!
//! A `.noeb` can also be **appended to a copy of the runtime binary** to make a single
//! self-contained executable (`noeta build --exe`). The layout is the runtime image, then the
//! bundle, then a fixed 16-byte trailer `[bundle_len: u64 LE | "NOEBEXE\0"]`. On startup the
//! `noeta` binary reads only that trailer (a cheap seek to end, not the whole image) and, if the
//! sentinel is present, runs the embedded bundle instead of the toolchain CLI. See [`staple`] /
//! [`extract_stapled`] / [`stapled_len`].

use noeta_bytecode::Module;

/// The four magic bytes every `.noeb` starts with.
pub const MAGIC: &[u8; 4] = b"NOEB";

/// The container format version this crate reads and writes.
///
/// Bumped to 2 when `Op::Call`/`Op::CallGlobal` gained their supplied-mask field: that changes the
/// serialized [`Module`] layout, and `RUNTIME_VERSION` alone does not catch it during development,
/// where the package version stays put across such a change. Without the bump a `.noeb` written by
/// an earlier build passes the version gate and is then postcard-decoded against the new layout —
/// a silent misread. The gate turns that into an explicit `UnsupportedFormat`.
///
/// Bumped to 3 when `reflect::ParamSig` gained its `optional` flag. `Module::reflection` is part of
/// the postcard payload, and postcard is **not** self-describing: a struct is its fields back to
/// back with no names or tags, so an extra `bool` shifts every byte after it. A version-2 bundle
/// decoded by a version-3 reader would read the next parameter's name length as the flag and
/// desynchronise from there — a corrupt manifest, not a clean error. Same reasoning as the bump
/// before it: any change to the serialized shape, however additive it looks in Rust, is a format
/// break on the wire.
///
/// Bumped to 4 when `Op::Invoke` gained the free-function form and its `recv` register became an
/// `Option<Reg>`. Same non-self-describing-encoding reasoning: an `Option` writes a discriminant
/// byte ahead of the register, so a version-3 reader would take that byte *as* the receiver
/// register and desynchronise for the rest of the chunk. `Module::code` is part of the payload, so
/// an op-layout change is a format break exactly as a manifest change is.
///
/// Parameter attributes, by contrast, did **not** bump it, and that is the same rule read the other
/// way round: they added no field to any serialized struct. A parameter's `#[...]` attributes ride
/// as ordinary rows in `reflection.manifest`, a `Vec<AttributeRecord>` whose element shape is
/// untouched — a longer vector, not a different layout, and postcard length-prefixes vectors. Nor
/// can an earlier artifact be *stale* the way a layout change makes one: a bundle written before
/// that slice came from a compiler that could not parse an attribute in a parameter list, so its
/// source cannot have contained one, and the rows it lacks are rows its program never had.
///
/// Bumped to 5 by the packed-widths arc, which added variants to three serialized enums in the
/// postcard payload — all non-self-describing, so a new variant shifts every discriminant after it.
/// `NarrowTarget` (embedded in `Op::As`/`Op::TypeTest`, part of `Module::code`) gained an `F32` head
/// for reified `f32` narrowing; `PackedFieldDef` (in `Module::packed_schemas`) gained `F64` and
/// `IntN { bits, signed }`; and `reflect::TypeRepr` (baked into `Op` narrow targets and
/// construction-site tags) gained the matching `F64` and `IntN { signed, bits }`. A version-4 bundle
/// decoded by a version-5 reader would map the old discriminants onto the wrong variants — a `Bool`
/// head read as `F32`, a `Struct` field read as an `IntN` — and desynchronise the chunk. Same rule
/// as the bumps before it: any change to a serialized enum's variant set is a wire break.
///
/// Bumped to 6 by the packed-widths **bare-scalar** arc: `PackedSchemaDef::shape` (in
/// `Module::packed_schemas`) changed from `u32` to `Option<u32>` so a bare-scalar `List<i32>`/`List<f32>`
/// element can carry *no* shape (it materializes to a bare `int`/`f32`, not an object). postcard prefixes
/// an `Option` with a present/absent discriminant byte, so the field's encoding — and every byte after it
/// in the schema table — shifts; a version-5 reader would misread the leading `u32` as an `Option` tag.
///
/// Bumped to 7 by the debugger top-level-locals arc: `Module` gained a trailing `global_bindings:
/// Vec<GlobalId>` (the top-level value-binding slots the debugger shows on the `main` frame). It is
/// empty in a release bundle, but postcard appends the (zero-length) sequence unconditionally, so
/// the payload grows by one byte and a version-6 reader would run off the end of the previous field.
///
/// Bumped to 8 by the struct-reflection arc: `reflect::TypeInfo` (in `Module::reflection`) gained two
/// trailing `Vec`s parallel to `fields` — `field_types: Vec<TypeRepr>` and `field_optional:
/// Vec<bool>` — so the type-level `field_specs_of` query can report each field's precise declared type
/// and optionality. Same non-self-describing-encoding reasoning as the bumps before it: postcard
/// writes the two sequences back to back after the existing fields with no tag, so a version-7 reader
/// decoding a version-8 payload would read their length prefixes as the next `TypeInfo`'s bytes and
/// desynchronise the manifest. (`field_specs_of` / `construct` add new `Op` variants too, but those
/// only appear in bundles this reader also produced.)
///
/// Bumped to 9 by the precise-trait-narrowing arc: `reflect::ReflectionInfo` (in
/// `Module::reflection`) gained a trailing `trait_impls: Vec<TraitImplRecord>` — the membership
/// table the now-precise `x is dyn Trait` / `x.as<dyn Trait>()` and the new `traits_of(value)`
/// read — and `NarrowTarget` (in `Module::code`) gained a `DynTrait(String)` variant. Postcard is
/// not self-describing, so the appended sequence and the new enum variant are both wire breaks by
/// the same reasoning as every bump above.
///
/// Bumped to 10 by two arcs landing together — one bump, because version 9 was never released and
/// either change alone is a wire break.
///
/// Bumped to 10 by the json-defaults arc: `TypeRecipe::Fielded::fields` (in `Module::deserialize_recipes`
/// and in the `Op` variants that carry a call-site recipe) changed from `Vec<(String, TypeRecipe)>` to
/// `Vec<FieldRecipe>` — each field now also carries its `FieldDefault`, so a JSON decode can fill a
/// field whose declaration gave it a literal default. Postcard writes the struct's fields back to back
/// with no tag, so the extra per-field enum shifts every byte after the first field; a version-9 reader
/// would take the `FieldDefault` discriminant as the next field's name length and desynchronise.
///
/// Bumped to 10 by the `returns_of` arc: `reflect::ParamRecord` (in `Module::reflection`) gained a
/// trailing `ret: TypeRepr` — a callable's declared return type, the projection the new
/// `returns_of(target)` query materializes. Postcard writes it back to back after `params` with no
/// tag, so a version-9 reader decoding a version-10 payload would take the return type's variant
/// discriminant as the next `ParamRecord`'s target-string length and desynchronise the manifest —
/// the same wire break as every bump above. `Op` (in `Module::code`) also gained a `ReturnsOf`
/// variant, declared beside `ParamsOf` rather than at the end, which shifts every discriminant after
/// it — a wire break on its own by the same non-self-describing-encoding rule.
/// Bumped to 11 by the enum-reflection arc: `reflect::VariantInfo` (in `Module::reflection`) gained
/// two trailing fields parallel to and beside `fields` — `field_types: Vec<TypeRepr>` and `backing:
/// Option<AttrValue>` — so the new type-level `variants_of` query can report a variant's payload
/// types precisely and a backed enum's wire value. Same non-self-describing-encoding reasoning as
/// every bump above: postcard writes them back to back after `fields` with no tag, so a version-10
/// reader would read the type sequence's length prefix as the next `VariantInfo`'s name length and
/// desynchronise the manifest. `Op` (in `Module::code`) also gained a `VariantsOf` variant, declared
/// beside `FieldSpecsOf` rather than at the end, which shifts every discriminant after it — a wire
/// break on its own by the same rule.
///
/// Bumped to 12 by the generic-forwarding arc: `Op::Call` and `Op::CallGlobal` (in `Module::code`)
/// each gained a `type_args: Box<[Reg]>` between `args` and `span`, and `Chunk` gained a `hidden:
/// u16` after `num_params` — the type-argument channel that replaced prepending a forwarding
/// generic's hidden slots onto the value-argument list. Same non-self-describing-encoding rule as
/// every bump above: postcard writes the new sequence's length prefix where a version-11 reader
/// expects the span, and the extra `u16` shifts every byte after it in each prototype header, so an
/// older artifact decoded here would desynchronise mid-chunk rather than fail cleanly.
///
/// Bumped to 13 by the same arc's Axis A and its cache-line budget: `Op::CallMethod` gained a
/// `type_args` of its own (methods forward now), `Chunk` gained `hidden_base`, and the three call
/// ops' `supplied` changed from `Option<u64>` to `Option<NonZeroU64>` to keep `Op` inside one cache
/// line. The last is a wire break even though the *meaning* is identical: postcard writes an
/// `Option`'s discriminant byte followed by the payload either way, but a niche-optimised `Option`
/// is not guaranteed to encode as the plain one, and the two ops around it moved regardless.
///
/// Bumped to 14 by the narrow-over-a-type-parameter fix: `Op::Narrow` and `Op::IsType` each gained
/// a `dynamic: Option<Reg>` — the register carrying the instantiation's runtime head name — placed
/// after `target` and so before `Narrow`'s two shape indices. Same non-self-describing-encoding rule
/// as every bump above: postcard writes the `Option`'s discriminant byte where a version-13 reader
/// expects `some_shape`'s first byte, desynchronising the rest of the stream.
///
/// Bumped to 15 by generic-in-generic construction: `Module` gained `type_arg_reprs: Vec<Option<u32>>`
/// — the reflection projection of `type_args`, so a construction whose instantiation arrives on a
/// hidden slot can resolve the interned `TypeRepr` that slot names — and `Op` (in `Module::code`)
/// gained a `RetagDynamic` variant, declared beside `Retag` rather than at the end. Both are wire
/// breaks by the same non-self-describing-encoding rule as every bump above: the new module table's
/// length prefix lands where a version-14 reader expects `names`, and the inserted `Op` variant shifts
/// every discriminant after it. This arrived alongside the 14 above rather than after it, so 15 is
/// what rejects an artifact either change alone would have labelled 14.
///
/// Bumped to 16 by the construct-guards arc: `reflect::TypeInfo` (in `Module::reflection`) gained a
/// `field_public: Vec<bool>` parallel to `fields`, between `field_optional` and `field_defaults` — the
/// per-field visibility the reflective construction door reads to refuse setting a private field (the
/// E0035 rule the checker enforces at a literal). Same non-self-describing-encoding reasoning as
/// version 8, which added the two `Vec`s beside it: postcard writes the sequence's length prefix
/// where a reader of the previous version expects `field_defaults`, so the whole manifest
/// desynchronises from that field on rather than failing cleanly. This landed independently of the 14
/// and 15 above — all three were in flight at once, and each is its own wire break — so 16 is what
/// rejects an artifact any one of them alone would have mislabelled.
/// Bumped to 17 by the swap-reflection-fidelity fix: `reflect::ReflectionInfo` (in
/// `Module::reflection`) gained a `role_tags: Vec<RoleTagRecord>` between `roles` and `params` — the
/// `@role(Enum.Variant)` tags as they ride on the *attribute declaration*, which `roles` is now
/// explicitly the derived join of. They are carried rather than discarded because the tag and the
/// use of the attribute are separable declarations and so land in different hot-swap fragments: with
/// only the join stored, a fragment re-declaring an annotated function purged its role binding and
/// had nothing to put back. Same non-self-describing-encoding rule as every bump above — postcard
/// writes the new sequence's length prefix where a version-16 reader expects `params`, so the whole
/// tail desynchronises rather than failing cleanly.
///
/// Bumped to 18 by the reflection-operand unification: `Op::AttributesOf` (in `Module::code`)
/// changed from `{ dst, type_name: NameId, dynamic: Option<Reg> }` to `{ dst, src: Reg }`, and
/// `Op::RolesOf` from `{ dst, role_enum: Option<NameId> }` to `{ dst, src: Option<Reg> }`. Both are
/// wire breaks by the same non-self-describing-encoding rule as every bump above — postcard writes a
/// `NameId` as a varint and a `Reg` as a byte, and the dropped `Option` discriminant shifts every
/// byte after it, so a version-17 reader decoding these ops walks off into the following
/// instruction.
///
/// The reason is that the two ops could not consume what the rest of the surface produces.
/// `Op::AttributesOf::dynamic` held an *index* into `Module::type_args` and resolved the name itself,
/// while `Op::TypeArgName`/`Op::TypeSlotName` — the two per-instantiation channels every other
/// name-keyed query reads — produce a **string**. So a type parameter arriving on the receiver's
/// reflected tag was rejected at `attributes_of::<T>()` and answered at `field_specs_of::<T>()`, and
/// `roles_of::<E>()` had no register at all. One name-string operand is what `Op::FieldSpecsOf`,
/// `Op::VariantsOf` and `Op::Construct` already take; the two channels fill it identically.
///
/// Bumped to 19 by class deserialization: a `class` decodes from JSON, so [`noeta_ext_abi::TypeRecipe::Struct`] became `Fielded`
/// and gained a [`noeta_ext_abi::FieldedKind`] saying which kind to build. A baked deserialize
/// recipe therefore carries one more field, and postcard writes an enum variant by *index*: adding
/// a field shifts every byte after it, and the two `Fielded`/`Enum` arms did not move, so a
/// version-18 reader decoding a version-19 recipe reads the kind byte as the start of
/// `has_validator` and everything after it slides.
///
/// The kind cannot be re-derived on the reading side — a recipe carries the type's *name*, and the
/// backend interns a shape from it without ever consulting a declaration — which is precisely why
/// it had to enter the artifact rather than be looked up.
/// Bumped to 20 by `#[Transient]`: [`noeta_object::Shape`] gained `transient_slots` (the slots the
/// deep marshal omits) and [`noeta_ext_abi::FieldRecipe`] gained `skipped`, so both the shape table
/// and every baked deserialize recipe grew a field. postcard writes a struct's fields back to back
/// with no tags, so a version-19 reader reads the new field's bytes as whatever follows it — and
/// silently, since the values stay well-formed. `TypeRecipe` also gained a `Transient` arm, which
/// appends to the variant-index space rather than shifting it; the field additions are what require
/// the bump.
///
/// Neither can be re-derived on the reading side: a shape carries slot *names* and a recipe carries
/// the type's name, and nothing downstream of the compiler ever sees the declaration that said which
/// fields do not leave the program.
/// Bumped to 21 by the `fields_of` visibility answer: [`noeta_bytecode::Op::FieldsOf`] gained
/// `private_fields`, the checker's per-site "may this door report private fields" bit. `Module::code`
/// is part of the postcard payload and an op is its fields back to back, so the added `bool` shifts
/// every byte after it in the chunk — a version-20 reader would take it as the start of the next op.
///
/// It cannot be re-derived on the reading side, which is the reason it travels: the answer depends on
/// the *call site's* enclosing type and package, and neither survives into the artifact.
/// Bumped to 22 by the unsigned render hint: [`noeta_bytecode::Op::Stringify`] gained `hint`, an
/// `Option<Box<noeta_ast::RenderHint>>` naming the positions of a display site's unsigned 64-bit
/// integers. It is an op field, so — exactly as for `FieldsOf` above — a version-21 reader would
/// take its bytes as the start of the next op and every op after it slides.
///
/// It cannot be re-derived on the reading side: the signedness of a fixed-width integer lives only
/// in the static type, which the checker resolves and the artifact does not otherwise carry.
///
/// Bumped to 23 by the JSON half of that hint, which moved the layout twice over.
/// [`noeta_bytecode::Op::JsonStringify`] is a new op carrying the [`noeta_ast::RenderHint`] a JSON
/// door serializes under: a new opcode renumbers nothing on its own, but a version-22 reader has no
/// arm for it and would take its operands as the start of the next op — the same slide, from the
/// other direction. And [`noeta_ext_abi::TypeRecipe::IntN`] (a fixed-width integer's decode recipe)
/// is **inserted** rather than appended, so every recipe variant after it shifts by one — a
/// version-22 artifact's `Float` recipe would decode as `IntN`. Neither can be re-derived on the
/// reading side: both say what the *static type* was, which the artifact does not otherwise carry.
///
/// Bumped to 24 by the ORDERING half of the same hint, which added a field at two levels.
/// [`noeta_object::Shape`] gained `unsigned_slots` — the slots a structural compare reads unsigned,
/// so a `@derive(Comparable)` type with a `u64` field orders by its value rather than by the
/// negative word it is erased to — and [`noeta_bytecode::Module`] gained `order_hint_sites`, the
/// span-keyed unsigned-position hints the VM reads at `.sorted()`/`.min()`/`.max()`/`.keys()`/
/// `.values()` and at a `for` over a set or map. postcard writes a struct's fields back to back with
/// no tags, so a version-23 reader takes each new field's bytes as whatever follows it — silently,
/// since the values stay well-formed. It cannot be re-derived on the reading side for the same
/// reason the two halves above cannot: signedness lives only in the static type.
///
/// **23 was claimed twice.** The display fix's two follow-ups — JSON and ordering — were written
/// against version 22 in parallel and each bumped it to 23, so for as long as they sat on separate
/// branches two different layouts wore one number. That is precisely the failure this constant
/// exists to prevent, and it is why the second to land takes 24 rather than keeping its own bump:
/// a version number is a claim about a byte layout, and only one layout may ever hold a number.
///
/// Bumped to 25 by the LAST position of that hint: the one with no serializing call to sit on.
/// [`noeta_bytecode::Module`] gained `binding_hint_sites`, the span-keyed hints the VM hands a native
/// dispatch that BINDS a value now and serializes it on a later tick — a LiveView binding, pushed
/// afresh on every flush, where the call site that knew the value was a `u64` is long gone. postcard
/// writes a struct's fields back to back with no tags, so a version-24 reader takes the new field's
/// bytes as whatever follows it, silently and still well-formed — the same slide `order_hint_sites`
/// caused at 24. It cannot be re-derived on the reading side for the reason every row above cannot:
/// signedness lives only in the static type.
/// Bumped to 26 by the hint's one remaining position: the door whose static type names a **type
/// parameter**. [`noeta_bytecode::Module`] gained `type_arg_hints` — each interned instantiation's
/// [`noeta_ext_abi::TypeArgHints`], read when a door inside a generic body resolves which width the
/// call bound its parameter to — and [`noeta_bytecode::Op::Stringify`]/`JsonStringify` carry a
/// [`noeta_bytecode::HintOperand`] (the hint plus the registers holding the frame's hidden
/// type-argument slots) where they carried a bare [`noeta_ast::RenderHint`], as does
/// `Module::order_hint_sites`. postcard writes a struct's fields back to back with no tags, so a
/// version-25 reader takes the new field's bytes as whatever follows it and reads a hint operand's
/// register list as the next op — the same slide every row above describes. And
/// [`noeta_ast::RenderHint`] gained a `Param` variant, which renumbers nothing that precedes it but
/// has no arm in a version-25 reader. None of it can be re-derived on the reading side, for the
/// reason every row above cannot: signedness lives only in the static type, and here it lives in
/// the *call's* static type rather than the door's.
/// Bumped to 27 by the two positions of that hint that have **no call frame to resolve against**.
/// [`noeta_bytecode::Op`] gained `SelfRenderSlot`, declared beside `TypeSlotName` — a door inside a
/// generic *type's* instance method reads its instantiation off the receiver's reflected tag, since
/// a method carries no hidden slot — and every discriminant after it shifts, which is the same wire
/// break the inserted `RetagDynamic` was at version 15. `Op::ResolveOrderHint` became
/// `Op::ResolveHint` with a leading `door` field, because a **kept** hint (a LiveView binding, which
/// serializes on a later tick with no frame left) is now spliced at the call that binds the value
/// and resolved through the same op: postcard writes the new field's byte where a version-26 reader
/// expects the slot list's length prefix. And the type-argument table's reflection projection
/// (`Module::type_arg_reprs` through `Module::type_reprs`) records a fixed-width scalar at TAG
/// fidelity — `IntN { signed, bits }` rather than `Int` — so a receiver's tag argument can be
/// matched against it; the bytes of an affected entry differ, and no reader can tell which fidelity
/// it is looking at. None of it can be re-derived on the reading side, for the reason every row
/// above cannot: signedness lives only in the static type.
/// Bumped to 28 by the last position of that hint: the instantiation a generic body **builds** out
/// of its own type parameters and hands to another generic — `wrap([v])` inside `fn built<T>(v: T)`,
/// which instantiates `wrap` at `List<T>`. Nothing in `built`'s signature names `List<T>`, so no
/// slot of `built` carries it whole and no caller could have interned a type the body invents; the
/// leaf is on a slot, though, and the shape around it is static, so [`noeta_bytecode::Op`] gained
/// `ComposeTypeArg` — declared beside `SelfRenderSlot`, so every discriminant after it shifts, which
/// is the same wire break the inserted `SelfRenderSlot` was at version 27. Its `cases` (the
/// checker's precomputed answer per combination of leaf values) are carried on the op, so no module
/// table moves. It cannot be re-derived on the reading side, for the reason every row above cannot:
/// signedness lives only in the static type — and here in a static type no signature spells.
pub const FORMAT_VERSION: u8 = 28;

/// The SHA-256 of one canonical [`Module`]'s postcard encoding — the *other* half of
/// [`FORMAT_VERSION`], and the thing that makes the changelog above enforceable.
///
/// Every paragraph up there was written after the fact by someone who noticed. One of them says so
/// out loud: `28e5d724b fix(bundle): bump the container format — the mask changed the Module
/// layout` is a `fix(` commit, because the bump was forgotten in the change that needed it. Nothing
/// connected the two halves, and the round-trip tests structurally cannot: they encode and decode
/// with the same build, so a layout change is invisible to them.
///
/// `crates/noeta-bundle/tests/module_layout_digest.rs` builds a `Module` from struct literals with
/// **every field named** and no `..Default::default()` anywhere in the reachable graph, encodes it,
/// and compares the hash to this constant. That gives two guards for one artifact: adding a field
/// anywhere reachable from `Module` — `Chunk`, `Shape`, `reflect::TypeInfo`, `TypeRecipe`, any of
/// them — stops *compiling* at that one site, and reordering two same-typed fields or changing a
/// field's type moves the bytes and so this digest. The second is the one a golden `.noeb` compared
/// by re-encoding would miss: postcard is positional, so swapping two `u32`s round-trips clean.
///
/// **Both must move together.** A digest updated without a `FORMAT_VERSION` bump re-greens the test
/// and leaves every previously-written artifact silently mis-decodable, which is the failure this
/// pair exists to prevent. The test's message says so; this doc says so; the changelog paragraph
/// you are about to write is the third place.
pub const MODULE_LAYOUT_DIGEST: &str =
    "aa55f592d9f73adb83579b8ed8664228b48fd8a983ec817daa9a5775973163ba";

/// The runtime version stamped into and checked against artifacts — the building crate's
/// package version. Any release that changes the serialized [`Module`] layout bumps this, so a
/// mismatch is the signal to rebuild the bundle.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `flags` bit 0: the payload is obfuscated (deflate-compressed + scrambled, P-AOT L1.4). Set on
/// every bundle [`write`] emits.
pub const FLAG_COMPRESSED: u8 = 1 << 0;
/// `flags` bit 1: reserved for a future encrypted payload. Access control / encrypt-at-rest is
/// intentionally out of scope for the build tool (application policy — see the module docs), so no
/// writer sets this and a reader rejects any bundle that does.
pub const FLAG_ENCRYPTED: u8 = 1 << 1;

/// The fixed seed for the obfuscation keystream (P-AOT L1.4). Not a secret — obfuscation only; it
/// lives in this open-source runtime. Chosen arbitrarily (no significance to the value).
const SCRAMBLE_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Why a byte slice is not a loadable `.noeb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// The blob is shorter than a valid header, or truncated mid-header.
    Truncated,
    /// The first four bytes are not [`MAGIC`] — not a `.noeb` at all.
    BadMagic,
    /// The container format version is newer/older than this reader supports.
    UnsupportedFormat { found: u8, supported: u8 },
    /// The artifact was built by a different runtime version (v1 pins artifacts to their builder).
    VersionMismatch { built: String, current: String },
    /// A `flags` transform (compression/encryption) this reader does not implement is set.
    UnsupportedTransform { flags: u8 },
    /// The payload did not deserialize into a `Module` (corrupt or malformed).
    Decode,
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::Truncated => write!(f, "not a valid .noeb: truncated header"),
            BundleError::BadMagic => write!(f, "not a .noeb bundle (bad magic)"),
            BundleError::UnsupportedFormat { found, supported } => write!(
                f,
                "unsupported .noeb format version {found} (this runtime reads {supported})"
            ),
            BundleError::VersionMismatch { built, current } => write!(
                f,
                "bundle was built by noeta {built}, but this runtime is {current} — rebuild the bundle"
            ),
            BundleError::UnsupportedTransform { flags } => write!(
                f,
                "bundle uses an unsupported transform (flags {flags:#04b}); this runtime cannot decode it"
            ),
            BundleError::Decode => write!(f, "corrupt .noeb payload (could not decode the module)"),
        }
    }
}

impl std::error::Error for BundleError {}

/// Serialize `module` into an obfuscated `.noeb` bundle: the versioned header followed by the
/// deflate-compressed, scrambled module payload (`FLAG_COMPRESSED`). See the module docs for what
/// obfuscation does and does not protect.
pub fn write(module: &Module) -> Vec<u8> {
    let rt = RUNTIME_VERSION.as_bytes();
    let payload = obfuscate(&module.encode());
    let mut out = Vec::with_capacity(4 + 3 + rt.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.push(FLAG_COMPRESSED);
    out.push(rt.len() as u8);
    out.extend_from_slice(rt);
    out.extend_from_slice(&payload);
    out
}

/// Parse and validate a `.noeb` bundle back into a [`Module`], or explain why it cannot be loaded.
pub fn read(bytes: &[u8]) -> Result<Module, BundleError> {
    // magic(4) + fmt_ver(1) + flags(1) + rt_len(1) = 7-byte minimum header.
    if bytes.len() < 7 {
        return Err(BundleError::Truncated);
    }
    if &bytes[0..4] != MAGIC {
        return Err(BundleError::BadMagic);
    }
    let fmt_ver = bytes[4];
    if fmt_ver != FORMAT_VERSION {
        return Err(BundleError::UnsupportedFormat {
            found: fmt_ver,
            supported: FORMAT_VERSION,
        });
    }
    let flags = bytes[5];
    // Encryption (L1.5) is not implemented here; any other unknown bit is a future transform.
    if flags & !FLAG_COMPRESSED != 0 {
        return Err(BundleError::UnsupportedTransform { flags });
    }
    let rt_len = bytes[6] as usize;
    let rt_end = 7 + rt_len;
    if bytes.len() < rt_end {
        return Err(BundleError::Truncated);
    }
    let built = std::str::from_utf8(&bytes[7..rt_end]).map_err(|_| BundleError::Truncated)?;
    if built != RUNTIME_VERSION {
        return Err(BundleError::VersionMismatch {
            built: built.to_string(),
            current: RUNTIME_VERSION.to_string(),
        });
    }
    let payload = &bytes[rt_end..];
    let encoded = if flags & FLAG_COMPRESSED != 0 {
        deobfuscate(payload)?
    } else {
        payload.to_vec()
    };
    Module::decode(&encoded).map_err(|_| BundleError::Decode)
}

/// Deflate-compress then scramble a raw module payload (P-AOT L1.4).
fn obfuscate(encoded: &[u8]) -> Vec<u8> {
    let mut compressed = miniz_oxide::deflate::compress_to_vec(encoded, 8);
    scramble(&mut compressed);
    compressed
}

/// The largest module payload [`deobfuscate`] will inflate to, in bytes.
///
/// Deflate amplifies: a payload of a few kilobytes can expand without bound, so an unbounded
/// inflate turns a corrupt or hostile bundle into an out-of-memory abort rather than a rejection.
/// That matters here because a bundle is read on paths with no supervision to speak of — the
/// startup cache reads one on *every* run, and the wasm runner loads one at the edge.
///
/// 256 MiB is about two orders of magnitude above any real module (they run to kilobytes and low
/// megabytes: bytecode, shapes, and a method table), so nothing that legitimately builds can reach
/// it, while a bomb is refused with the same [`BundleError::Decode`] any other bad payload gets.
const MAX_INFLATED: usize = 256 * 1024 * 1024;

/// Reverse [`obfuscate`]: de-scramble then inflate, refusing a payload that expands past
/// [`MAX_INFLATED`]. `Err` on a corrupt/foreign payload.
fn deobfuscate(payload: &[u8]) -> Result<Vec<u8>, BundleError> {
    let mut buf = payload.to_vec();
    scramble(&mut buf); // XOR is its own inverse
    miniz_oxide::inflate::decompress_to_vec_with_limit(&buf, MAX_INFLATED)
        .map_err(|_| BundleError::Decode)
}

/// XOR `buf` in place with a SplitMix64 keystream seeded from [`SCRAMBLE_SEED`] — its own inverse.
/// A byte-level scramble so the shipped payload is not literally "just deflate, inflate it";
/// obfuscation only (the seed is public), not encryption.
fn scramble(buf: &mut [u8]) {
    let mut state = SCRAMBLE_SEED;
    let mut keystream = 0u64;
    for (i, byte) in buf.iter_mut().enumerate() {
        if i % 8 == 0 {
            // SplitMix64: advance and mix a fresh 64-bit keystream word every 8 bytes.
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            keystream = z ^ (z >> 31);
        }
        *byte ^= (keystream >> ((i % 8) * 8)) as u8;
    }
}

/// Whether `bytes` begins with the `.noeb` magic — a cheap sniff for the CLI to decide between
/// "run this bundle" and "compile this source file", without a full parse.
pub fn is_bundle(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == MAGIC
}

/// The sentinel closing a stapled-executable trailer (P-AOT L2). Placed at the very end of a
/// `noeta build --exe` artifact so startup can detect an embedded bundle by reading the tail alone.
pub const EXE_MAGIC: &[u8; 8] = b"NOEBEXE\0";

/// The fixed trailer size a stapled executable ends with: an 8-byte little-endian bundle length
/// followed by [`EXE_MAGIC`].
pub const TRAILER_LEN: usize = 16;

/// Append `bundle` (the bytes from [`write`]) to a copy of the runtime image `runtime`, producing a
/// self-contained executable (P-AOT L2). The bundle sits between the untouched runtime image and a
/// locating trailer, so the OS still sees a valid executable while startup can recover the bundle.
pub fn staple(runtime: &[u8], bundle: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(runtime.len() + bundle.len() + TRAILER_LEN);
    out.extend_from_slice(runtime);
    out.extend_from_slice(bundle);
    out.extend_from_slice(&(bundle.len() as u64).to_le_bytes());
    out.extend_from_slice(EXE_MAGIC);
    out
}

/// If `trailer` is exactly the final [`TRAILER_LEN`] bytes of a stapled executable, return the
/// embedded bundle's length in bytes. `None` for the plain runtime binary (no sentinel) — the
/// startup fast path that avoids reading the whole image just to learn there is no bundle.
pub fn stapled_len(trailer: &[u8]) -> Option<usize> {
    if trailer.len() != TRAILER_LEN || &trailer[8..] != EXE_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(trailer[0..8].try_into().ok()?);
    Some(len as usize)
}

/// Recover the embedded bundle from a whole stapled-executable `image`, or `None` if it carries no
/// trailer (a plain runtime binary). A convenience over [`stapled_len`] for callers that already
/// hold the full bytes (tests); the CLI seeks the tail instead of reading the whole binary.
pub fn extract_stapled(image: &[u8]) -> Option<&[u8]> {
    if image.len() < TRAILER_LEN {
        return None;
    }
    let (body, trailer) = image.split_at(image.len() - TRAILER_LEN);
    let bundle_len = stapled_len(trailer)?;
    body.len()
        .checked_sub(bundle_len)
        .map(|start| &body[start..])
}

// Wasm stapling (P-WASM W1.2) — the `noeta build --wasm` analogue of [`staple`]: instead of a
// tail trailer (a wasm guest cannot read its own binary), the bundle is injected into the wasm
// runner's data section and a compiled-in slot is patched to point at it. The patcher is a
// dependency-free section-level rewrite, so it compiles everywhere this crate does (the runner
// included — dead code there, stripped by the linker).
mod wasm;
pub use wasm::{WasmStapleError, staple_wasm};

/// The 16-byte marker the wasm runner's bundle slot starts with (P-WASM W1.2) — the
/// patcher↔runner contract. Slot layout: `magic, ptr: u32 LE, len: u32 LE`; the runner keeps
/// exactly one copy in its data section (the slot initializer), and `staple_wasm` refuses to
/// patch zero or several occurrences.
pub const WASM_SLOT_MAGIC: [u8; 16] = *b"NOETA_BUNDLE_SLT";

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_bytecode::Module;

    /// A minimal but non-empty module round-trips through the container.
    fn tiny_module() -> Module {
        // The compiler is not a dependency here; an empty module exercises the header + payload
        // path. Corpus-wide module coverage lives in the conformance bundle oracle (L1.0/L1.3).
        Module::default()
    }

    #[test]
    fn write_then_read_recovers_the_module() {
        let m = tiny_module();
        let blob = write(&m);
        assert!(is_bundle(&blob));
        let back = read(&blob).expect("valid bundle");
        assert_eq!(back.encode(), m.encode());
    }

    /// `Module` has no `PartialEq` (its ops carry none), so tests assert on the error side only.
    fn err(bytes: &[u8]) -> BundleError {
        read(bytes).expect_err("expected a rejection")
    }

    #[test]
    fn bad_magic_is_rejected() {
        assert_eq!(err(b"not a bundle at all"), BundleError::BadMagic);
    }

    #[test]
    fn truncated_is_rejected() {
        assert_eq!(err(b"NOE"), BundleError::Truncated);
        // Valid magic + fmt but a rt_len claiming more bytes than present.
        let mut blob = write(&tiny_module());
        blob.truncate(5);
        assert_eq!(err(&blob), BundleError::Truncated);
    }

    #[test]
    fn version_mismatch_is_reported() {
        let mut blob = write(&tiny_module());
        // Corrupt the stored runtime-version bytes (rt starts at offset 7).
        blob[7] = blob[7].wrapping_add(1);
        assert!(matches!(err(&blob), BundleError::VersionMismatch { .. }));
    }

    #[test]
    fn the_encryption_flag_is_unsupported_in_v1() {
        // L1.4 supports FLAG_COMPRESSED; the keyed-encryption bit (L1.5) is still rejected.
        let mut blob = write(&tiny_module());
        blob[5] |= FLAG_ENCRYPTED;
        assert!(matches!(
            err(&blob),
            BundleError::UnsupportedTransform { .. }
        ));
    }

    #[test]
    fn a_stapled_bundle_round_trips_out_of_a_runtime_image() {
        // A fake "runtime image" with a real bundle stapled on: the trailer locates the bundle and
        // `extract_stapled` recovers exactly the bytes `write` produced.
        let runtime = b"pretend this is a big ELF binary".as_slice();
        let bundle = write(&tiny_module());
        let image = staple(runtime, &bundle);
        assert_eq!(
            &image[..runtime.len()],
            runtime,
            "runtime image is untouched"
        );
        let recovered = extract_stapled(&image).expect("trailer present");
        assert_eq!(recovered, bundle.as_slice());
        // And the recovered bundle loads back into a module.
        let back = read(recovered).expect("embedded bundle is valid");
        assert_eq!(back.encode(), tiny_module().encode());
    }

    #[test]
    fn a_plain_runtime_image_has_no_stapled_bundle() {
        // No trailer sentinel ⇒ the startup fast path reports "no bundle" from the tail alone.
        let runtime = b"pretend this is a big ELF binary with no bundle".as_slice();
        assert!(extract_stapled(runtime).is_none());
        assert!(stapled_len(&runtime[runtime.len() - TRAILER_LEN..]).is_none());
        // Too short to even hold a trailer.
        assert!(extract_stapled(b"tiny").is_none());
    }

    /// A payload that inflates past [`MAX_INFLATED`] is rejected, not allocated.
    ///
    /// Deflate amplifies enormously on repetitive input — the bundle built here is a few kilobytes
    /// and would expand to 512 MiB — and the inflate used to be unbounded, so a corrupt or hostile
    /// `.noeb` was an out-of-memory abort rather than an error return. On the paths that read one
    /// (the startup cache, every run; the wasm runner, at the edge) that is a crash with no
    /// diagnosis. Found while fuzzing the container: the readers were total over every corruption
    /// tried, but totality says nothing about how much memory the answer costs.
    #[test]
    fn a_decompression_bomb_is_refused_rather_than_allocated() {
        // Twice the cap, of the most compressible thing there is.
        let bomb = vec![0u8; MAX_INFLATED * 2];
        let blob = {
            let rt = RUNTIME_VERSION.as_bytes();
            let mut out = Vec::new();
            out.extend_from_slice(MAGIC);
            out.push(FORMAT_VERSION);
            out.push(FLAG_COMPRESSED);
            out.push(rt.len() as u8);
            out.extend_from_slice(rt);
            out.extend_from_slice(&obfuscate(&bomb));
            out
        };
        // The whole bundle is tiny — that is the point of a bomb.
        assert!(
            blob.len() < 1024 * 1024,
            "the bomb did not compress, so this test is not testing what it says"
        );
        assert!(
            matches!(read(&blob), Err(BundleError::Decode)),
            "an over-large payload must be refused like any other bad one"
        );
    }

    #[test]
    fn the_payload_is_obfuscated_not_plaintext_bytecode() {
        let m = tiny_module();
        let blob = write(&m);
        // The compression flag is set…
        assert_eq!(blob[5] & FLAG_COMPRESSED, FLAG_COMPRESSED);
        // …and the on-disk payload is not the raw postcard encoding (a transform was applied).
        let rt_len = blob[6] as usize;
        let payload = &blob[7 + rt_len..];
        assert_ne!(
            payload,
            m.encode().as_slice(),
            "payload must be transformed"
        );
    }
}
