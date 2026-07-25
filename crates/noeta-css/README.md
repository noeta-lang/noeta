# noeta-css

A first-party **CSS tier-body formatter** for `noeta fmt`, in its own namespace (not `std`).

- **Takes in:** a `<style>` tier body (already hole-free — `noeta-html`'s `sub`-delegation hands it over) plus the indent to place it under.
- **Emits:** a formatter-only [`Extension`] (`CssExtension`, namespace root `"css"`) registering a `"css"` body formatter over the pure-Rust [`malva`](https://docs.rs/malva) CSS formatter.

An HTML formatter (`noeta-html`) reflows structure but leaves `<style>` content verbatim, because CSS is a different language. This crate closes that gap the extension way: `noeta-html` delegates a `<style>` body here, and the result is placed back under the tag, reindented. Registering it is opt-in — a toolchain that doesn't want the CSS dependency simply doesn't install this extension, and `<style>` bodies stay verbatim.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
