//! `noeta-graphbench` — run the graph-retrieval benchmark.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use noeta_graphbench::arms::Arm;
use noeta_graphbench::baseline::{self, Baseline};
use noeta_graphbench::harness::{self, Prepared};
use noeta_graphbench::question::{self, Category};
use noeta_graphbench::service::{Service, Tool, args};
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

    /// Call one MCP tool against the first selected project's entry and print its raw JSON.
    /// The evidence half of a diagnosis: what the wire said, beside what the gold holds.
    #[arg(long, value_name = "TOOL")]
    dump_tool: Option<Tool>,

    /// The `symbol` argument a `--dump-tool` call carries, for the tools that take one.
    #[arg(long, value_name = "NAME")]
    dump_symbol: Option<String>,

    /// Another `--dump-tool` argument, as `key=value`. Repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    dump_arg: Vec<String>,

    /// Measure what the node `id` object costs on the wire, and how much of each answer states a
    /// fact the id already states. Reports; changes nothing.
    #[arg(long)]
    token_cost: bool,

    /// Ask `context_map` for this ranking instead of its default: `ppr`, `degree` or `random`.
    /// The ranking ablation: run it three ways and read the recall column.
    #[arg(long, value_name = "RANKER")]
    ranker: Option<String>,

    /// Ask `path` for this ranking instead of its default: `flow` or `shortest`.
    #[arg(long, value_name = "RANKER")]
    path_ranker: Option<String>,

    /// Spend this token budget on the map arms instead of the 4k they are defined at. A budget
    /// large enough to hold a project's whole connected component measures nothing about ranking,
    /// so this is how a ranking is measured under a budget that binds.
    #[arg(long, value_name = "TOKENS")]
    map_budget: Option<usize>,
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
    service.set_rankers(&noeta_graphbench::service::Rankers {
        context_map: cli.ranker.clone(),
        path: cli.path_ranker.clone(),
        map_budget: cli.map_budget,
    });

    if cli.token_cost {
        let code = token_cost(&mut service, &prepared).await;
        service.shutdown().await;
        return code;
    }

    if let Some(tool) = cli.dump_tool {
        let code = dump_tool(
            &mut service,
            &prepared,
            tool,
            cli.dump_symbol.as_deref(),
            &cli.dump_arg,
        )
        .await;
        service.shutdown().await;
        return code;
    }

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
    // A run that asked for a different ranking measured a different configuration, so it neither
    // holds the baseline nor may overwrite it. Saying so is the point of the ablation: the numbers
    // are read against the default run's, by a reader, not by a floor.
    if cli.ranker.is_some() || cli.path_ranker.is_some() || cli.map_budget.is_some() {
        println!(
            "\ngraphbench: this run changed the ranking configuration ({}), so it is neither \
             compared against the baseline nor recorded into it. Read its rows against a plain \
             run's.",
            [
                cli.ranker.as_ref().map(|r| format!("context_map={r}")),
                cli.path_ranker.as_ref().map(|r| format!("path={r}")),
                cli.map_budget.map(|b| format!("map_budget={b}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        );
        return Ok(ExitCode::SUCCESS);
    }
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
            .every_requirement()
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

/// Measure the identity's cost on the wire, over one call to every tool that reports nodes.
///
/// The same calls the arms make, read a second way: not for what they answer but for what they
/// weigh. Both halves of the answer are billed to an agent's context, so the id and the fields that
/// repeat it are counted apart.
async fn token_cost(service: &mut Service, prepared: &[Prepared]) -> Result<ExitCode, String> {
    use noeta_graphbench::tokens::Ledger;
    let mut ledger = Ledger::default();
    for project in prepared {
        let entry = serde_json::json!(project.context.entry.display().to_string());
        // A symbol every project has a declaration for, so the symbol-addressed tools answer.
        let symbol = project
            .facts
            .nodes
            .iter()
            .find(|node| !node.is_test && !node.leaf.is_empty())
            .map(|node| node.qualified.clone())
            .unwrap_or_default();
        let calls: [(Tool, Vec<(&str, serde_json::Value)>); 7] = [
            (
                Tool::Symbols,
                vec![
                    ("file", entry.clone()),
                    ("scope", serde_json::json!("workspace")),
                ],
            ),
            (Tool::ModuleGraph, vec![("file", entry.clone())]),
            (Tool::Reflect, vec![("file", entry.clone())]),
            (
                Tool::Trace,
                vec![
                    ("file", entry.clone()),
                    ("from", serde_json::json!("EntryPoint")),
                ],
            ),
            (
                Tool::Definition,
                vec![
                    ("file", entry.clone()),
                    ("symbol", serde_json::json!(symbol.clone())),
                ],
            ),
            (
                Tool::Callers,
                vec![
                    ("file", entry.clone()),
                    ("symbol", serde_json::json!(symbol.clone())),
                ],
            ),
            (
                Tool::References,
                vec![
                    ("file", entry.clone()),
                    ("symbol", serde_json::json!(symbol.clone())),
                ],
            ),
        ];
        for (tool, pairs) in calls {
            let mut request = serde_json::Map::new();
            for (name, value) in pairs {
                request.insert(name.to_string(), value);
            }
            let answer = service.call(tool, request).await;
            ledger.record(tool.as_str(), &answer);
        }
    }
    print!("{}", ledger.render());
    Ok(ExitCode::SUCCESS)
}

/// Call one tool against a project's entry and print what came back, unread by any adapter.
///
/// The adapter is the only code that interprets a tool's JSON, which means a wrong answer and a
/// misread answer look identical from the report. This prints the wire itself, so a diagnosis can
/// say which of the two it is.
async fn dump_tool(
    service: &mut Service,
    prepared: &[Prepared],
    tool: Tool,
    symbol: Option<&str>,
    extra: &[String],
) -> Result<ExitCode, String> {
    let project = prepared
        .first()
        .ok_or_else(|| "no corpus project matched".to_string())?;
    let entry = project.context.entry.display().to_string();
    let mut request = args([("file", serde_json::json!(entry))]);
    if let Some(symbol) = symbol {
        request.insert("symbol".to_string(), serde_json::json!(symbol));
    }
    for pair in extra {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("`{pair}` is not a `key=value` argument"))?;
        let parsed: serde_json::Value =
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value));
        request.insert(key.to_string(), parsed);
    }
    let answer = service.call(tool, request).await;
    println!(
        "graphbench: {tool} on {} (root {})",
        project.context.name,
        project.context.root.display()
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&answer).unwrap_or_else(|_| answer.to_string())
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
