# reserve/ — crates.io name-reservation placeholders

These are **throwaway placeholder crates** whose only purpose is to claim names
on [crates.io](https://crates.io) before Noeta's first tagged release. Each is
its own independent workspace root (note the empty `[workspace]` table in its
`Cargo.toml`), so it publishes in isolation and does **not** participate in the
main `lang-*` workspace one directory up.

| Crate       | Reserves                          |
| ----------- | --------------------------------- |
| `noeta`     | the embeddable library entry point |
| `noeta-cli` | the CLI crate + the `noeta` command |

## Publishing (do this once each)

```sh
cd reserve/noeta      && cargo publish
cd ../noeta-cli       && cargo publish
```

`cargo publish --dry-run` validates packaging without uploading.

Once published, `0.0.0` permanently holds the name (published versions can be
yanked but never deleted or reassigned). At the first real release these will
be replaced by — or promoted into — the actual crates, and this directory can
be removed.
