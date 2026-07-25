# noeta-bundle

The `.noeb` bundle container (P-AOT L1.1): a versioned envelope around a serialized `noeta_bytecode::Module`, so a compiled program can be shipped and run without its `.noe` source.

- **Takes in:** a `Module` (from `noeta-bytecode`).
- **Emits:** the `.noeb` byte format (`write`/`read`), plus [`staple`]/[`extract_stapled`]/[`stapled_len`] for appending a bundle onto a copy of the runtime binary to make a single self-contained executable.

This crate owns the artifact *format* only — magic, versioning, and the obfuscation transform — and is deliberately isolated from the core mid-end crates so those never pull the container's compression/crypto dependencies. The header records a `fmt_ver` (the container layout) and an embedded `rt_ver` (the runtime that built the artifact); since the postcard payload is not self-describing, `read` rejects a `rt_ver` mismatch outright rather than risk misdecoding a stale layout. The default payload is deflate-compressed and XOR-scrambled (`FLAG_COMPRESSED`) — honestly-labeled obfuscation against casual inspection (`noeta dump`, a hex editor), not encryption; a reserved `FLAG_ENCRYPTED` bit stays for forward compatibility. A stapled executable (`noeta build --exe`) appends `[runtime image | bundle | 16-byte trailer]`; the `noeta` binary itself reads only that trailer on startup to detect and run an embedded bundle.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
