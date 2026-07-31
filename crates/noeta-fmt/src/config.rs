//! Reading [`FmtConfig`] from a project's `noeta.toml` `[fmt]` table — the single config parser,
//! shared by the CLI (`noeta fmt`) and the LSP formatting provider so their output cannot drift.

use std::path::Path;

use crate::{ArrowStyle, FmtConfig, ParenStyle, SemicolonStyle};

impl FmtConfig {
    /// Overlay a `noeta.toml`'s `[fmt]` table (if any) onto [`FmtConfig::default`]. Unknown keys
    /// inside `[fmt]` are ignored (room for later knobs); known keys are type-checked and
    /// `match_arm_arrows` is validated against its allowed values. `Err` only on a malformed `[fmt]`
    /// table, so a typo surfaces rather than being silently ignored.
    pub fn from_toml(text: &str) -> Result<FmtConfig, String> {
        let mut config = FmtConfig::default();
        config.overlay_toml(text)?;
        Ok(config)
    }

    /// Overlay a `noeta.toml`'s `[fmt]` table onto `self` in place (leaving unset keys as-is), so it
    /// can be applied *on top of* `.editorconfig` — an explicit `noeta.toml` setting wins.
    pub fn overlay_toml(&mut self, text: &str) -> Result<(), String> {
        let table: toml::Table = text.parse().map_err(|err| format!("{err}"))?;
        let Some(fmt_value) = table.get("fmt") else {
            return Ok(());
        };
        let fmt = fmt_value.as_table().ok_or("`fmt` must be a table")?;

        if let Some(v) = fmt.get("wrap") {
            self.wrap = v.as_bool().ok_or("`fmt.wrap` must be a boolean")?;
        }
        if let Some(v) = fmt.get("line_width") {
            let n = v
                .as_integer()
                .filter(|n| *n > 0)
                .ok_or("`fmt.line_width` must be a positive integer")?;
            self.line_width = n as usize;
        }
        if let Some(v) = fmt.get("match_arm_arrows") {
            self.match_arm_arrows = match v.as_str() {
                Some("compact") => ArrowStyle::Compact,
                Some("align") => ArrowStyle::Align,
                _ => {
                    return Err(
                        "`fmt.match_arm_arrows` must be \"compact\" or \"align\"".to_string()
                    );
                }
            };
        }
        if let Some(v) = fmt.get("sort_imports") {
            self.sort_imports = v.as_bool().ok_or("`fmt.sort_imports` must be a boolean")?;
        }
        if let Some(v) = fmt.get("parens") {
            self.parens = v
                .as_str()
                .and_then(|s| s.parse::<ParenStyle>().ok())
                .ok_or("`fmt.parens` must be \"remove\" or \"add\"")?;
        }
        if let Some(v) = fmt.get("semicolons") {
            self.semicolons = v
                .as_str()
                .and_then(|s| s.parse::<SemicolonStyle>().ok())
                .ok_or("`fmt.semicolons` must be \"remove\", \"add\", or \"preserve\"")?;
        }
        if let Some(v) = fmt.get("indent_width") {
            let n = v
                .as_integer()
                .filter(|n| *n > 0)
                .ok_or("`fmt.indent_width` must be a positive integer")?;
            self.indent_width = n as usize;
        }
        if let Some(v) = fmt.get("indent_style") {
            self.use_tabs = match v.as_str() {
                Some("space") => false,
                Some("tab") => true,
                _ => return Err("`fmt.indent_style` must be \"space\" or \"tab\"".to_string()),
            };
        }
        if let Some(v) = fmt.get("insert_final_newline") {
            self.final_newline = v
                .as_bool()
                .ok_or("`fmt.insert_final_newline` must be a boolean")?;
        }
        if let Some(v) = fmt.get("trim_trailing_whitespace") {
            self.trim_trailing = v
                .as_bool()
                .ok_or("`fmt.trim_trailing_whitespace` must be a boolean")?;
        }
        Ok(())
    }

    /// Overlay `.editorconfig` settings for `path` onto `self`. Meant to be applied *before*
    /// [`overlay_toml`](Self::overlay_toml), so an explicit `noeta.toml [fmt]` setting wins over
    /// `.editorconfig`, which in turn wins over the built-in defaults. Honors `indent_style`,
    /// `indent_size`, `max_line_length`, `insert_final_newline`, and `trim_trailing_whitespace`
    /// (`end_of_line`/`charset` are not applicable — LF/UTF-8 only). Lenient: any error leaves `self`
    /// unchanged, so formatting never fails on a `.editorconfig` problem.
    pub fn overlay_editorconfig(&mut self, path: &Path) {
        let Ok(mut props) = ec4rs::properties_of(path) else {
            return;
        };
        props.use_fallbacks();
        if let Some(v) = props
            .get_raw_for_key("indent_style")
            .filter_unset()
            .into_option()
        {
            match v {
                "tab" => self.use_tabs = true,
                "space" => self.use_tabs = false,
                _ => {}
            }
        }
        if let Some(v) = props
            .get_raw_for_key("indent_size")
            .filter_unset()
            .into_option()
            && let Ok(n) = v.parse::<usize>()
            && n > 0
        {
            self.indent_width = n;
        }
        if let Some(v) = props
            .get_raw_for_key("max_line_length")
            .filter_unset()
            .into_option()
            && let Ok(n) = v.parse::<usize>()
            && n > 0
        {
            self.line_width = n;
        }
        if let Some(v) = props
            .get_raw_for_key("insert_final_newline")
            .filter_unset()
            .into_option()
        {
            self.final_newline = v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = props
            .get_raw_for_key("trim_trailing_whitespace")
            .filter_unset()
            .into_option()
        {
            self.trim_trailing = v.eq_ignore_ascii_case("true");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_fmt_table() {
        assert_eq!(
            FmtConfig::from_toml("[targets.dev.tiers]\ntest = true\n").unwrap(),
            FmtConfig::default()
        );
    }

    #[test]
    fn overlays_known_keys() {
        let c = FmtConfig::from_toml(
            "[fmt]\nwrap = true\nline_width = 80\nmatch_arm_arrows = \"align\"\nsort_imports = true\nparens = \"add\"\nsemicolons = \"preserve\"\n",
        )
        .unwrap();
        assert!(c.wrap);
        assert_eq!(c.line_width, 80);
        assert_eq!(c.match_arm_arrows, ArrowStyle::Align);
        assert!(c.sort_imports);
        assert_eq!(c.parens, ParenStyle::Add);
        assert_eq!(c.semicolons, SemicolonStyle::Preserve);
    }

    #[test]
    fn paren_and_semicolon_defaults_are_remove() {
        let c = FmtConfig::default();
        assert_eq!(c.parens, ParenStyle::Remove);
        assert_eq!(c.semicolons, SemicolonStyle::Remove);
    }

    #[test]
    fn overlays_indentation_and_newline_keys() {
        let c = FmtConfig::from_toml(
            "[fmt]\nindent_width = 2\nindent_style = \"tab\"\ninsert_final_newline = false\ntrim_trailing_whitespace = false\n",
        )
        .unwrap();
        assert_eq!(c.indent_width, 2);
        assert!(c.use_tabs);
        assert!(!c.final_newline);
        assert!(!c.trim_trailing);
        assert!(FmtConfig::from_toml("[fmt]\nindent_style = \"tabs\"\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nindent_width = 0\n").is_err());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(FmtConfig::from_toml("[fmt]\nwrap = 1\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nline_width = 0\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nmatch_arm_arrows = \"aligned\"\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nparens = \"keep\"\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nsemicolons = \"maybe\"\n").is_err());
    }
}
