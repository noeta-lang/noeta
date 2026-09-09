//! `noeta-graphbench` — run the graph-retrieval benchmark.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use noeta_graphbench::arms::Arm;
use noeta_graphbench::baseline::{self, Baseline};
use noeta_graphbench::harness::{self, Prepared};
use noeta_graphbench::question::{self, Category};
use noeta_graphbench::service::Service;
use noeta_graphbench::{ablate, generate};

/// How well a composition of `noeta mcp` tool calls answers a graph question about a Noeta project.
#[derive(Debug, Parser)]
#[command(name = "noeta-graphbench", version, about, long_about = None)]
struct Cli {
    /// Rewrite `tests/graphbench/baseline.txt` from this run instead of comparing against it.
    #[arg(long)]
    record: bool,

    /// Also write the full per-question report here.
    #[arg(long, value_name = "PATH")]
    json: Option<PathBuf>,

    /// Run only the projects and categories whose name contains this.
    #[arg(long, value_name = "SUBSTRING")]
    only: Option<String>,

    /// Run only these arms (repeatable).
    #[arg(long, value_name = "ARM")]
    arm: Vec<Arm>,

    /// Print the corpus, the questions and the arms, and measure nothing.
    #[arg(long)]
    list: bool,

    /// Stub one tool to its empty answer and report which categories collapse. `all` sweeps every
    /// stubbable tool and fails when a category survives all of them.
    #[arg(long, value_name = "TOOL")]
    ablate: Option<String>,

    /// Generate a synthetic project of this many declarations, with its gold graph, and measure
    /// against it. Repeatable.
    #[arg(long, value_name = "DECLARATIONS")]
    synthetic: Vec<usize>,

    /// Where synthetic projects are written (a temporary directory by default).
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// Print a project's declaration universe and stop. What a seed question is labeled against.
    #[arg(long, value_name = "PROJECT")]
    dump_gold: Option<String>,

    /// Write every question, with its gold as `file#name` labels, for the agent-layer arm.
    #[arg(long, value_name = "PATH")]
    emit_questions: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(problem) => {
            eprintln!("graphbench: {problem}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    let data = noeta_graphbench::data_dir();
    if !cli.synthetic.is_empty() {
        return generate::measure(&cli.synthetic, cli.out_dir.as_deref()).await;
    }

    let prepared = harness::prepare(&data, cli.only.as_deref())?;
    if let Some(project) = &cli.dump_gold {
        return dump_gold(&prepared, project);
    }
    if let Some(path) = &cli.emit_questions {
        return emit_questions(&prepared, path);
    }

    let arms: Vec<Arm> = if cli.arm.is_empty() {
        Arm::ALL.to_vec()
    } else {
        cli.arm.clone()
    };
    let categories: Vec<Category> = Category::ALL
        .into_iter()
        .filter(|category| match &cli.only {
            None => true,
            Some(only) => {
                category.as_str().contains(only.as_str())
                    || prepared
                        .iter()
                        .any(|p| p.context.name.contains(only.as_str()))
            }
        })
        .collect();

    if cli.list {
        print_plan(&prepared, &arms, &categories);
        return Ok(ExitCode::SUCCESS);
    }

    let mut service = Service::start().await?;

    if let Some(which) = &cli.ablate {
        let code = ablate::run(&mut service, &prepared, &arms, &categories, which).await;
        service.shutdown().await;
        return code;
    }

    let report = harness::measure(&mut service, &prepared, &arms, &categories).await;
    service.shutdown().await;
    print!("{}", report.render());

    if let Some(path) = &cli.json {
        report.write_json(path)?;
        println!("graphbench: wrote {}", path.display());
    }

    let baseline_path = data.join("baseline.txt");
    if cli.record {
        std::fs::write(&baseline_path, baseline::record(&report))
            .map_err(|e| format!("cannot write {}: {e}", baseline_path.display()))?;
        println!("graphbench: recorded {}", baseline_path.display());
        return Ok(ExitCode::SUCCESS);
    }
    if cli.only.is_some() || !cli.arm.is_empty() {
        println!(
            "\ngraphbench: a filtered run measures a subset of the baseline's rows, so it is not \
             compared against it."
        );
        return Ok(ExitCode::SUCCESS);
    }

    let baseline = Baseline::read(&baseline_path)?;
    let regressions = baseline::compare(&report, &baseline);
    if regressions.is_empty() {
        println!("\ngraphbench: every row holds its baseline.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("\ngraphbench: {} row(s) below baseline", regressions.len());
    for regression in &regressions {
        println!(
            "  {} {}: {}",
            regression.arm.as_str(),
            regression.category.as_str(),
            regression.what
        );
    }
    println!("  re-record with:  cargo run -p noeta-graphbench -- --record");
    Ok(ExitCode::FAILURE)
}

fn print_plan(prepared: &[Prepared], arms: &[Arm], categories: &[Category]) {
    println!("graphbench: corpus");
    for project in prepared {
        println!(
            "  {:<18} {:>3} files  {:>4} declarations  {:>3} questions",
            project.context.name,
            project.context.files.len(),
            project.facts.nodes.len(),
            project.questions.len(),
        );
    }
    println!("\ngraphbench: arms");
    for arm in arms {
        let needs = arm
            .requires()
            .iter()
            .map(|tool| tool.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {:<4} {:<46} {}",
            arm.as_str(),
            arm.label(),
            if needs.is_empty() {
                "no extra tools".to_string()
            } else {
                format!("needs {needs}")
            }
        );
    }
    println!("\ngraphbench: categories");
    for category in categories {
        let count = prepared
            .iter()
            .flat_map(|p| p.questions.iter())
            .filter(|q| q.category == *category)
            .count();
        let negatives = prepared
            .iter()
            .flat_map(|p| p.questions.iter())
            .filter(|q| q.category == *category && q.negative)
            .count();
        println!(
            "  {:<14} {:>4} questions ({negatives} negative)  {}",
            category.as_str(),
            count,
            if category.ranked() {
                "ranked (Acc@k, MRR)"
            } else {
                "set (P/R/F1)"
            }
        );
    }
}

/// Write every question with its gold as `file#name` labels — the input the agent-layer arm reads.
fn emit_questions(prepared: &[Prepared], path: &std::path::Path) -> Result<ExitCode, String> {
    #[derive(serde::Serialize)]
    struct Emitted<'a> {
        id: &'a str,
        project: &'a str,
        category: &'a str,
        prompt: &'a str,
        entry: String,
        root: String,
        negative: bool,
        gold: Vec<String>,
    }
    let mut out = Vec::new();
    for project in prepared {
        for q in &project.questions {
            out.push(Emitted {
                id: &q.id,
                project: &q.project,
                category: q.category.as_str(),
                prompt: &q.prompt,
                entry: project.context.entry.display().to_string(),
                root: project.context.root.display().to_string(),
                negative: q.negative,
                gold: question::gold_labels(&project.facts, q),
            });
        }
    }
    let text = serde_json::to_string_pretty(&out)
        .map_err(|e| format!("cannot serialize the questions: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!(
        "graphbench: wrote {} questions to {}",
        out.len(),
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Print a project's declaration universe, which is what a seed question is labeled against.
fn dump_gold(prepared: &[Prepared], project: &str) -> Result<ExitCode, String> {
    let found = prepared
        .iter()
        .find(|p| p.context.name == project)
        .ok_or_else(|| format!("`{project}` is no corpus project"))?;
    for node in &found.facts.nodes {
        println!(
            "{:<9} {:<46} {:<38} roles={:<30} test={} callers={} tests={}",
            node.kind.as_str(),
            node.qualified,
            question::normalize_file(&node.file),
            node.roles.join(","),
            node.is_test,
            found.facts.callers(node.id).len(),
            found.facts.impacted_tests(node.id).len(),
        );
    }
    println!("\nmodules:");
    for (module, file) in &found.facts.modules {
        println!(
            "  {:<34} {:<30} importers={}",
            module,
            question::normalize_file(file),
            found.facts.importers(module).len()
        );
    }
    Ok(ExitCode::SUCCESS)
}
