//! CVSS v3.x base-score computation (advisory-intake residual b) — the client half of the registry's
//! `src/cvss.ts`. An imported advisory carries the CVSS vector its severity band was derived from (an
//! unsigned, informational field the feed echoes); `noeta audit` re-derives the base **score** from that
//! vector so it can show both the band and the number behind it.
//!
//! Only the **base** metric group is scored — the part every upstream vector carries. The equations are
//! the published CVSS v3.1 base-metric formulas (FIRST CVSS v3.1 specification §7.1); the qualitative
//! band follows the standard severity-rating scale (§5). Deterministic and dependency-free — MUST stay
//! byte-for-byte equivalent to the TypeScript implementation so a band the registry stored matches the
//! score the client shows.

/// The qualitative severity band for a CVSS base score (CVSS v3.1 §5, Table 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CvssBand {
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl CvssBand {
    pub fn as_str(&self) -> &'static str {
        match self {
            CvssBand::None => "none",
            CvssBand::Low => "low",
            CvssBand::Medium => "medium",
            CvssBand::High => "high",
            CvssBand::Critical => "critical",
        }
    }
}

struct BaseMetrics {
    av: f64,
    ac: f64,
    pr_raw: char,
    ui: f64,
    scope_changed: bool,
    c: f64,
    i: f64,
    a: f64,
}

fn av_weight(v: &str) -> Option<f64> {
    match v {
        "N" => Some(0.85),
        "A" => Some(0.62),
        "L" => Some(0.55),
        "P" => Some(0.2),
        _ => None,
    }
}
fn ac_weight(v: &str) -> Option<f64> {
    match v {
        "L" => Some(0.77),
        "H" => Some(0.44),
        _ => None,
    }
}
fn ui_weight(v: &str) -> Option<f64> {
    match v {
        "N" => Some(0.85),
        "R" => Some(0.62),
        _ => None,
    }
}
fn cia_weight(v: &str) -> Option<f64> {
    match v {
        "H" => Some(0.56),
        "L" => Some(0.22),
        "N" => Some(0.0),
        _ => None,
    }
}
fn pr_weight(raw: char, scope_changed: bool) -> f64 {
    match (raw, scope_changed) {
        ('N', _) => 0.85,
        ('L', false) => 0.62,
        ('L', true) => 0.68,
        ('H', false) => 0.27,
        ('H', true) => 0.5,
        _ => 0.85,
    }
}

/// Parse a CVSS v3.0/3.1 vector string into its base metrics, or `None` if it is not a well-formed v3
/// base vector (missing a mandatory base metric, an unknown value, or a non-v3 prefix). Extra
/// (temporal/environmental) metrics are tolerated and ignored.
fn parse_vector(vector: &str) -> Option<BaseMetrics> {
    let trimmed = vector.trim();
    let upper = trimmed.to_ascii_uppercase();
    let body: &str = if upper.starts_with("CVSS:3.0/") || upper.starts_with("CVSS:3.1/") {
        &trimmed[trimmed.find('/')? + 1..]
    } else if upper.starts_with("AV:") {
        trimmed
    } else {
        return None;
    };

    let mut m: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for part in body.split('/') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once(':')?;
        m.insert(k.to_ascii_uppercase(), v.to_ascii_uppercase());
    }
    let get = |k: &str| m.get(k).map(|s| s.as_str());

    let av = av_weight(get("AV")?)?;
    let ac = ac_weight(get("AC")?)?;
    let ui = ui_weight(get("UI")?)?;
    let c = cia_weight(get("C")?)?;
    let i = cia_weight(get("I")?)?;
    let a = cia_weight(get("A")?)?;
    let pr = get("PR")?;
    if !matches!(pr, "N" | "L" | "H") {
        return None;
    }
    let scope = get("S")?;
    let scope_changed = match scope {
        "U" => false,
        "C" => true,
        _ => return None,
    };
    Some(BaseMetrics {
        av,
        ac,
        pr_raw: pr.chars().next().unwrap(),
        ui,
        scope_changed,
        c,
        i,
        a,
    })
}

/// Round up to one decimal place, per the CVSS v3.1 spec's exact `Roundup` (Appendix A) — defined on
/// integers to avoid binary-float artefacts.
fn roundup(input: f64) -> f64 {
    let int_input = (input * 100_000.0).round() as i64;
    if int_input % 10_000 == 0 {
        int_input as f64 / 100_000.0
    } else {
        ((int_input as f64 / 10_000.0).floor() + 1.0) / 10.0
    }
}

fn base_score(m: &BaseMetrics) -> f64 {
    let iss = 1.0 - (1.0 - m.c) * (1.0 - m.i) * (1.0 - m.a);
    let impact = if m.scope_changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return 0.0;
    }
    let pr = pr_weight(m.pr_raw, m.scope_changed);
    let exploitability = 8.22 * m.av * m.ac * pr * m.ui;
    let raw = if m.scope_changed {
        1.08 * (impact + exploitability)
    } else {
        impact + exploitability
    };
    roundup(raw.min(10.0))
}

/// The qualitative band for a base score (CVSS v3.1 §5, Table 14).
pub fn band_for_score(score: f64) -> CvssBand {
    if score <= 0.0 {
        CvssBand::None
    } else if score < 4.0 {
        CvssBand::Low
    } else if score < 7.0 {
        CvssBand::Medium
    } else if score < 9.0 {
        CvssBand::High
    } else {
        CvssBand::Critical
    }
}

/// Parse a CVSS vector and derive `(score, band)`, or `None` if it is not a valid v3 base vector. The
/// single entry point `noeta audit` uses to display an imported advisory's CVSS score.
pub fn score_vector(vector: &str) -> Option<(f64, CvssBand)> {
    let m = parse_vector(vector)?;
    let score = base_score(&m);
    Some((score, band_for_score(score)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The published FIRST CVSS v3.1 values — mirrors the registry's cvss.test.ts so both sides agree.
    #[test]
    fn published_example_vectors_score_as_published() {
        let cases: &[(&str, f64, CvssBand)] = &[
            (
                "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H",
                9.8,
                CvssBand::Critical,
            ),
            (
                "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H",
                10.0,
                CvssBand::Critical,
            ),
            (
                "CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:U/C:L/I:N/A:N",
                4.3,
                CvssBand::Medium,
            ),
            (
                "CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:C/C:L/I:L/A:N",
                6.4,
                CvssBand::Medium,
            ),
            (
                "CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N",
                1.8,
                CvssBand::Low,
            ),
            (
                "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N",
                0.0,
                CvssBand::None,
            ),
            (
                "CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H",
                7.8,
                CvssBand::High,
            ),
        ];
        for (vector, score, band) in cases {
            let (got_score, got_band) = score_vector(vector).expect(vector);
            assert_eq!(got_score, *score, "score for {vector}");
            assert_eq!(got_band, *band, "band for {vector}");
        }
    }

    #[test]
    fn accepts_cvss30_and_bare_vectors() {
        assert_eq!(
            score_vector("CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H")
                .unwrap()
                .0,
            9.8
        );
        assert_eq!(
            score_vector("AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H")
                .unwrap()
                .0,
            9.8
        );
    }

    #[test]
    fn rejects_malformed_or_non_v3() {
        assert!(score_vector("").is_none());
        assert!(score_vector("nonsense").is_none());
        assert!(score_vector("CVSS:2.0/AV:N/AC:L").is_none());
        assert!(score_vector("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H").is_none());
        assert!(score_vector("CVSS:3.1/AV:X/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H").is_none());
    }
}
