# Arc: CLI completion — the I/O primitives and `para/cli`

The CLI-foundations arc (merged) built every *language-side* reflection prerequisite for a
signature-driven CLI framework (`BuiltinTy`, `#[Arg]`, `ParamInfo.optional`, free-function `invoke`,
`.{ }` literals, `env.get -> ?string`). What it never built is the **destination**: the host/std I/O
primitives a CLI needs, and the framework itself. This arc finishes it.

## Architectural finding (the constraint everything rides on)

Program output is **batch-captured, not streamed**. Both backends accumulate `self.stdout: String`
and return it in `RunResult { stdout, exit_code, diagnostics }`; the CLI does
`print!("{}", result.stdout)` **only after** the program finishes (`noeta-cli/src/cmd/run.rs`).
`echo` is a keyword (`EchoKw` → `Stmt::Echo`) that writes straight to that buffer (VM
`Op::Echo` → `self.out.stdout`; tree-walker `ir.rs` → `self.stdout`). There is **no** streaming
sink and **no** `std.io` module.

Consequences:
- stderr is *observable output* → it belongs in `RunResult` and the differential oracle, NOT as a
  host effect. Route it through the backends' buffers, exactly like stdout.
- stdin/TTY *are* host effects → they belong on a **Host capability** (like `Env`/`Os`), sandbox
  fixture vs `RealHost` real I/O, so the differential stays deterministic.
- Interactive prompt-then-read can't rely on a flush of the batch buffer. Give the stdin capability
  a `prompt(msg)` that writes to the real terminal and reads a line **immediately** — the one
  interactive path — while `echo`/`io.out`/`io.err` stay batch. Piped (non-interactive) stdin needs
  no flush and works under the batch model.

## Slice 1 — stderr + the `std.io` module (the blocker)

- `noeta-backend`: `RunResult` gains `stderr: String`.
- `noeta-ext-abi/src/ctx.rs`: `NativeCtx` gains `write_stdout(&mut self, &str)` +
  `write_stderr(&mut self, &str)`. Both backends (`noeta-vm`, `noeta-eval`) implement them by
  pushing to their stdout/stderr buffers. This is the seam that lets an ordinary native reach the
  compared output — no lowerer intrinsic, no bytecode/format change, `echo` untouched.
- `noeta-stdlib/src/registry.rs`: new **`std.io`** `ExtModule` — `out(x)` / `err(x)` (raw, no
  newline) and `outln(x)` / `errln(x)` (trailing `\n`). `echo` stays the stdout-line sugar.
- CLI: `run.rs` (and `repl.rs`, `serve.rs`) print `result.stderr` to real stderr.
- Conformance: `expectation.rs` + harness compare `stderr` too. Add a fixture that writes to both
  streams and asserts the split.
- **Gate:** 7-corpus differential + JIT differential + the new stderr fixture, both backends agree.

## Slice 2 — stdin + TTY (`Console` host capability)

- `noeta-ext-abi/src/host.rs`: new `Console` capability trait — `stdin_read_line(&mut) -> Option<String>`
  (None = EOF), `stdin_read_all(&mut) -> String`, `is_tty(Stream) -> bool` (stdin/stdout/stderr),
  `prompt(&mut self, msg: &str) -> Option<String>` (write msg to real stderr **now**, read one line).
- `SandboxHost`: scripted stdin fixture (deterministic lines), `is_tty` = false everywhere,
  `prompt` consumes the fixture. Keeps the differential deterministic.
- `RealHost` (`noeta-host-real`): real `std::io::stdin().lock()`, `IsTerminal`.
- `std.io` natives: `stdin_line() -> ?string`, `stdin_all() -> string`, `is_tty() -> bool`
  (stdout by convention; plus explicit variants if cheap), `prompt(msg) -> ?string`.
- **Gate:** sandbox fixture drives a read-loop program; both backends agree; corpus + JIT green.

## Slice 3 — `para/cli` (the framework)

- New package `packages/para-cli`, scope `para/cli`, pure Noeta (mirrors `para-api`/`para-html`).
- Command model off reflection: `#[Command]` on functions, `#[Arg]` on params; the runner reads
  `attributes_of` / `params_of` / `ParamInfo.optional` / `type_of` and dispatches via free-function
  `invoke(name, args)`. **The signature is the spec.**
- argv parsing: positionals + `--flag` / `--flag=value` + `-s` short + `--` terminator; `--help`
  generated from signatures; usage/errors → `io.err`; exit codes.
- Uses `.{ }` for option structs, `env.get`, `args.all()`, and slices 1–2.
- **Gate:** `@test` blocks exercising a sample multi-command CLI (parse, dispatch, help, error, exit).

## Coordination

Sequential (1→2→3): slice 2 needs `std.io` to exist; slice 3 needs stderr (and wants stdin/TTY).
Each slice = its own worktree off `arc-cli-complete`, its **own** `CARGO_TARGET_DIR` (shared target
= rlib contamination), fully gated, coordinator-reviewed, merged into `arc-cli-complete` from the
arc worktree before the next starts. Final `arc-cli-complete` → main after full-arc gate.
