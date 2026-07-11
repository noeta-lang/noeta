//! Reading [`FmtConfig`] from a project's `noeta.toml` `[fmt]` table — the single config parser,
//! shared by the CLI (`noeta fmt`) and the LSP formatting provider so their output cannot drift.

use std::path::Path;

use crate::{ArrowStyle, FmtConfig, ParenStyle, SemicolonStyle};

/// The project manifest that carries the `[fmt]` table.
const MANIFEST_NAME: &str = "noeta.toml";

impl FmtConfig {
    /// Overlay a `noeta.toml`'s `[fmt]` table (if any) onto [`FmtConfig::default`]. Unknown keys
    /// inside `[fmt]` are ignored (room for later knobs); known keys are type-checked and
    /// `match_arm_arrows` is validated against its allowed values. `Err` only on a malformed `[fmt]`
    /// table, so a typo surfaces rather than being silently ignored.
    pub fn from_toml(text: &str) -> Result<FmtConfig, String> {
        let table: toml::Table = text.parse().map_err(|err| format!("{err}"))?;
        let mut config = FmtConfig::default();

        let Some(fmt_value) = table.get("fmt") else {
            return Ok(config);
        };
        let fmt = fmt_value.as_table().ok_or("`fmt` must be a table")?;

        if let Some(v) = fmt.get("wrap") {
            config.wrap = v.as_bool().ok_or("`fmt.wrap` must be a boolean")?;
        }
        if let Some(v) = fmt.get("line_width") {
            let n = v
                .as_integer()
                .filter(|n| *n > 0)
                .ok_or("`fmt.line_width` must be a positive integer")?;
            config.line_width = n as usize;
        }
        if let Some(v) = fmt.get("match_arm_arrows") {
            config.match_arm_arrows = match v.as_str() {
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
            config.sort_imports = v.as_bool().ok_or("`fmt.sort_imports` must be a boolean")?;
        }
        if let Some(v) = fmt.get("parens") {
            config.parens = v
                .as_str()
                .and_then(|s| s.parse::<ParenStyle>().ok())
                .ok_or("`fmt.parens` must be \"remove\" or \"add\"")?;
        }
        if let Some(v) = fmt.get("semicolons") {
            config.semicolons = v
                .as_str()
                .and_then(|s| s.parse::<SemicolonStyle>().ok())
                .ok_or("`fmt.semicolons` must be \"remove\", \"add\", or \"preserve\"")?;
        }
        Ok(config)
    }

    /// Discover the nearest `noeta.toml` at or above `start_dir` and read its `[fmt]` config,
    /// **leniently**: a missing manifest, an unreadable file, or a malformed `[fmt]` table all yield
    /// [`FmtConfig::default`]. Suited to the editor path, where formatting should never fail on a
    /// config problem; the CLI uses [`FmtConfig::from_toml`] directly so it can report the error.
    pub fn discover(start_dir: &Path) -> FmtConfig {
        for dir in start_dir.ancestors() {
            let candidate = dir.join(MANIFEST_NAME);
            if candidate.is_file() {
                return std::fs::read_to_string(&candidate)
                    .ok()
                    .and_then(|text| FmtConfig::from_toml(&text).ok())
                    .unwrap_or_default();
            }
        }
        FmtConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_fmt_table() {
        assert_eq!(
            FmtConfig::from_toml("[targets.dev.tiers]\ntest = \"std\"\n").unwrap(),
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
    fn rejects_bad_values() {
        assert!(FmtConfig::from_toml("[fmt]\nwrap = 1\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nline_width = 0\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nmatch_arm_arrows = \"aligned\"\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nparens = \"keep\"\n").is_err());
        assert!(FmtConfig::from_toml("[fmt]\nsemicolons = \"maybe\"\n").is_err());
    }
}
