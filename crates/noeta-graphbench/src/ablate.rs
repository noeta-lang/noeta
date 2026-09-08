//! The benchmark's ablation of itself.
//!
//! A category that scores well while never reaching the tool it claims to use reads exactly like one
//! that works. So each ablation stubs one tool to its empty answer, re-runs the arms, and reports
//! which (arm, category) rows fell. A category no ablation moves is a category whose questions are
//! being answered by something other than the tools, and `--ablate all` fails when one is found.
//!
//! The summary separates two verdicts, because they mean different things. A category reached by
//! stubbing a **Noeta graph tool** is one the graph surface is really answering. A category reached
//! only by stubbing the **file read** is one where the lexical control carries the score and the
//! graph tools contribute nothing an ablation can take away, which is a finding about the tools
//! rather than about the benchmark.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::str::FromStr;

use crate::arms::Arm;
use crate::harness::{self, Prepared};
use crate::question::Category;
use crate::report::Status;
use crate::service::{Service, Tool};

/// How far an F1 must fall for an ablation to count as having reached the category.
pub const REACHED: f64 = 0.05;

/// Run one ablation, or the whole sweep.
pub async fn run(
    service: &mut Service,
    prepared: &[Prepared],
    wanted: &[Arm],
    categories: &[Category],
    which: &str,
) -> Result<ExitCode, String> {
    let baseline = harness::measure(service, prepared, wanted, categories).await;
    let base: BTreeMap<(Arm, Category), f64> = baseline
        .rows
        .iter()
        .filter(|row| row.status.scored())
        .map(|row| ((row.arm, row.category), row.score.f1))
        .collect();

    let tools: Vec<Tool> = if which == "all" {
        Tool::ablatable().to_vec()
    } else {
        vec![Tool::from_str(which)?]
    };

    println!(
        "graphbench: ablation (a category must fall by {REACHED:.2} F1 to count as reached)\n"
    );
    println!(
        "{:<12} {:<4} {:<14} {:>8} {:>8}  {}",
        "ablated", "arm", "category", "before", "after", "verdict"
    );
    let mut reached: BTreeSet<Category> = BTreeSet::new();
    let mut by_graph_tool: BTreeSet<Category> = BTreeSet::new();
    for tool in &tools {
        service.ablate(Some(*tool));
        let run = harness::measure(service, prepared, wanted, categories).await;
        for row in &run.rows {
            let Status::Scored = row.status else { continue };
            let Some(before) = base.get(&(row.arm, row.category)) else {
                continue;
            };
            let fell = *before - row.score.f1;
            if fell < REACHED {
                continue;
            }
            reached.insert(row.category);
            if *tool != Tool::FileRead {
                by_graph_tool.insert(row.category);
            }
            println!(
                "{:<12} {:<4} {:<14} {:>8.3} {:>8.3}  fell {:.3}",
                tool.as_str(),
                row.arm.as_str(),
                row.category.as_str(),
                before,
                row.score.f1,
                fell,
            );
        }
    }
    service.ablate(None);

    let scored: Vec<Category> = categories
        .iter()
        .copied()
        .filter(|c| base.keys().any(|(_, category)| category == c))
        .collect();
    let unreached: Vec<Category> = scored
        .iter()
        .copied()
        .filter(|c| !reached.contains(c))
        .collect();
    let lexical_only: Vec<Category> = scored
        .iter()
        .copied()
        .filter(|c| reached.contains(c) && !by_graph_tool.contains(c))
        .collect();
    println!(
        "\ngraphbench: {} of {} scored categories reached by an ablation, {} of them by a Noeta \
         graph tool",
        reached.len(),
        scored.len(),
        by_graph_tool.len(),
    );
    for category in &lexical_only {
        println!(
            "  {} moves only when the file read is stubbed — the lexical arm carries that score and \
             the graph tools add nothing an ablation can take away",
            category.as_str()
        );
    }
    if which != "all" {
        return Ok(ExitCode::SUCCESS);
    }
    if unreached.is_empty() {
        println!("graphbench: every scored category is reachable by at least one ablation.");
        return Ok(ExitCode::SUCCESS);
    }
    for category in &unreached {
        println!(
            "  {} survives every ablation — its questions are not reaching the tools they name",
            category.as_str()
        );
    }
    Ok(ExitCode::FAILURE)
}
