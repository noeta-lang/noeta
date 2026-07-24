# para/cli

A CLI framework for Noeta where **the function signature is the spec**. You annotate ordinary
functions with `#[Command]`; the framework discovers them by whole-program reflection, parses `argv`,
coerces each token to the parameter's declared type, dispatches the call by name, generates `--help`,
and turns the command's return value into a process exit code. No parser to configure, no argument
table to keep in sync with the code — the signature *is* the table.

## Usage

Add the dependency, keyed `para`:

```toml
[dependencies]
para = { path = "path/to/para-cli" }   # or a registry version
```

Import the names you use — the attribute structs must be imported **by name** for `#[Command]` /
`#[Arg]` to resolve:

```noeta
use para.cli.{Command, Arg, run}
use std.{os}

#[Command(about: "Add two integers")]
fn add(a: int, b: int): int {
    return a + b;   // an int return becomes the exit code
}

#[Command(about: "Greet someone")]
fn greet(name: string, #[Arg(short: "l", help: "shout it")] loud: bool = false): int {
    io.outln(if loud then "HELLO ${name}" else "hello ${name}");
    return 0;
}

// `noeta test` never runs top-level statements, so this line is the real entry only.
os.exit(run());
```

```
$ myprog add 2 3          # exit 5
$ myprog greet Ada -l     # HELLO Ada
$ myprog --help           # lists the commands
$ myprog greet --help     # usage for one command
```

## The surface

| symbol | kind | purpose |
| --- | --- | --- |
| `Command { about = "", name = "" }` | `@attribute(Function, Method)` | marks an invokable command; `name` overrides the derived name and MAY contain spaces (a nested path) |
| `Arg { help = "", short = "", long = "", env = "" }` | `@attribute(Param)` | per-parameter help, flag spellings, and an environment-variable fallback |
| `run(): int` | fn | dispatch the real `argv` (minus argv0), return the exit code |
| `dispatch(argv: List<string>): int` | fn | the testable core: dispatch a synthetic argv |

### Env-var fallback

A parameter's value is sourced in order: an explicit `argv` token (positional/named/short), else the
`#[Arg(env: "NAME")]` variable when set, else the parameter's own default (or a usage error if it is
required). The fallback is shown in `--help` as `[env: NAME]`.

```noeta
#[Command(about: "Bind a server port")]
fn serve(#[Arg(env: "PORT", help: "port to bind")] port: int = 8080): int { ... }
//  serve                     # $PORT if set, else 8080
//  PORT=9000 serve           # 9000
//  serve --port 3000         # 3000 (argv outranks env)
```

### Nested subcommands

`#[Command(name: "remote add")]` gives a command a multi-token path. Selection matches the **longest**
command-name token-prefix of `argv`, so `remote add …` beats a bare `remote`. Top-level `--help`
groups shared first-tokens hierarchically. A partial path with no matching command (e.g. a bare
`config` when only `config set`/`config get` exist) is a usage error (exit 2) that lists the valid
continuations.

### Colored help

When stdout is a terminal (`io.is_tty()`), `--help`/usage output is colorized — bold section headers,
dimmed types and annotations. Piped output is plain, so captured help carries no escape codes.

### Shell completions

`--completions <bash|zsh|fish>` prints a sourceable completion script to stdout (exit 0) that
completes the program's command-name tokens (nested paths included) and every command's flag names.
It is a framework flag, not a `completions` subcommand, so it never collides with a user command name.

```
$ myprog --completions bash > /etc/bash_completion.d/myprog
$ source <(myprog --completions zsh)
$ myprog --completions fish > ~/.config/fish/completions/myprog.fish
```

`run()` is `dispatch(args.all().slice(1, …))`. Test against `dispatch` directly — because a command's
`int` return is its exit code, an arithmetic command is self-verifying with no output to capture:

```noeta
@test {
    fn adds(): void { assert(dispatch(["add", "2", "3"]) == 5); }
    fn usage_error(): void { assert(dispatch(["add", "2"]) == 2); }   // missing required
}
```

## Behaviour

- **Command selection.** With several commands, the first non-flag token picks the subcommand; with
  exactly one command, there is no subcommand token — every argv token is that command's argument.
- **Argument forms.** `--long value`, `--long=value`, `-s value`, `-s=value`; a `bool` parameter is a
  flag (`--flag` presence, `--flag=false`, or `--no-flag`); `--` ends option parsing; remaining bare
  tokens fill the non-bool parameters in declaration order.
- **Coercion.** `int` via `to_int`, `float` via `to_float`, `bool` via `true`/`1`/`false`/`0`; any
  other type receives the raw string.
- **Optional parameters.** A parameter with a default may be omitted; the argument list is left short
  and the callee fills its own default.
- **Exit codes.** `0` success (or `--help`), `1` the command returned an `Err`, `2` a usage error
  (unknown command/option, missing required argument, bad coercion, too many positionals).

See `examples/para-cli/demo/` (multi-command) and `examples/para-cli/single/` (single-command).
