# noeta-html

A first-party **HTML tier-body formatter** for `noeta fmt`, in its own namespace (not `std`).

- **Takes in:** an HTML tier body with each `${…}` hole collapsed to a single NUL placeholder, plus the indent to lay the top level at.
- **Emits:** a formatter-only [`Extension`] (`HtmlExtension`, namespace root `"html"`) registering a native HTML re-indenter for the `"html"` language; `fmt` substitutes the reflowed holes back in and re-applies tier-body escaping.

`@html` is a program tier (declared in the in-language liveview package, with a reactive Noeta handler); this crate is the extension-driven tier-body-formatting story taken the rest of the way. Any tier — program or native — that declares `text: "html"` gets it; `fmt` resolves by language and delegates. Core stays HTML-ignorant, the `@html` handler stays idiomatic Noeta, and the formatter lives where the language knowledge does: here. The formatter is a pure foreign reflow — this crate never sees Noeta syntax, only HTML — distinguishing **block** elements (each gets its own line, children indented) from **inline** ones (flow on the current line).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
