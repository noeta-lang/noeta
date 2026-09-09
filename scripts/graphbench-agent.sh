#!/usr/bin/env bash
# The agent layer of the graph-retrieval benchmark: a real model, answering one question per
# invocation, with `noeta mcp` as its only tool server.
#
# The retrieval layer (`cargo run -p noeta-graphbench`) measures fixed tool compositions and is
# gated. This measures what a model does with the same tools, and is NIGHTLY ONLY. It is never
# gated, for two reasons that do not go away: an answer costs money, and the same question asked
# twice can come back differently. Its numbers belong in a report, not in a baseline file.
#
#   scripts/graphbench-agent.sh --arm A0 --limit 3
#   scripts/graphbench-agent.sh --arm A0 --category callers --out /tmp/agent-A0.jsonl
#
# WHAT IT DOES, per question:
#   1. Writes an MCP config naming the built `noeta` binary as an stdio server.
#   2. Runs `claude --print --output-format json` with a tool allowlist for the arm, so an arm that
#      is supposed to work from `symbols` and `references` cannot quietly reach for grep.
#   3. Reads the answer, the tool-call count and the token usage out of the JSON result.
#   4. Scores a structural category by exact set comparison against the gold labels the harness
#      emitted, and reports precision, recall and F1 per question and in total.
#
# WHAT IT FIXES, and what it cannot. `--model` pins the model. There is no temperature flag on the
# CLI, so runs vary; the fix is repeats, and the summary prints how many were run. Every question
# runs in its own invocation with no resumed session, so one answer cannot prime the next.
#
# PREREQUISITES: the `noeta` binary (`cargo build -p noeta-cli`), the `claude` CLI, `python3`, and
# the questions file, which this script regenerates unless one is passed with --questions.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARM="A0"
CATEGORY=""
PROJECT=""
LIMIT=3
MODEL="${GRAPHBENCH_AGENT_MODEL:-claude-sonnet-4-5}"
MAX_TURNS=24
OUT=""
QUESTIONS=""
NOETA_BIN="${NOETA_BIN:-}"

usage() {
    sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arm) ARM="${2:?--arm needs A0|A5}"; shift 2 ;;
        --category) CATEGORY="${2:?--category needs a name}"; shift 2 ;;
        --project) PROJECT="${2:?--project needs a name}"; shift 2 ;;
        --limit) LIMIT="${2:?--limit needs a count}"; shift 2 ;;
        --model) MODEL="${2:?--model needs a model id}"; shift 2 ;;
        --max-turns) MAX_TURNS="${2:?--max-turns needs a count}"; shift 2 ;;
        --out) OUT="${2:?--out needs a path}"; shift 2 ;;
        --questions) QUESTIONS="${2:?--questions needs a path}"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "graphbench-agent: unknown argument $1" >&2; usage 2 ;;
    esac
done

for tool in claude python3; do
    command -v "$tool" > /dev/null 2>&1 || { echo "graphbench-agent: $tool is not on PATH" >&2; exit 2; }
done

if [[ -z "$NOETA_BIN" ]]; then
    for candidate in "${CARGO_TARGET_DIR:-$ROOT/target}/debug/noeta" "${CARGO_TARGET_DIR:-$ROOT/target}/release/noeta"; do
        [[ -x "$candidate" ]] && NOETA_BIN="$candidate" && break
    done
fi
if [[ ! -x "$NOETA_BIN" ]]; then
    echo "graphbench-agent: no noeta binary. Build one: cargo build -p noeta-cli" >&2
    exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/graphbench-agent-XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
[[ -z "$OUT" ]] && OUT="$WORK/answers.jsonl"

if [[ -z "$QUESTIONS" ]]; then
    QUESTIONS="$WORK/questions.json"
    ( cd "$ROOT" && cargo run -q -p noeta-graphbench -- --emit-questions "$QUESTIONS" ) \
        > "$WORK/emit.log" 2>&1
    rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "graphbench-agent: could not emit questions (exit $rc)" >&2
        cat "$WORK/emit.log" >&2
        exit 2
    fi
fi

cat > "$WORK/mcp.json" <<EOF
{
  "mcpServers": {
    "noeta": { "command": "$NOETA_BIN", "args": ["mcp"] }
  }
}
EOF

# The allowlist is the arm. A0 gets the Noeta graph tools and a file read; A5 gets the file tools
# only, which is the grep control the literature calls for.
#
# Comma-separated, not space-separated: `--allowedTools` and `--mcp-config` are both variadic, so a
# space-separated list swallows the prompt that follows it and `claude` exits 1 reporting no input.
# The `--` before the prompt is the other half of that fix.
case "$ARM" in
    A0) ALLOWED="mcp__noeta__symbols,mcp__noeta__definition,mcp__noeta__references,mcp__noeta__trace,mcp__noeta__module_graph,mcp__noeta__reflect,Read" ;;
    A5) ALLOWED="Read,Grep,Glob" ;;
    *)  echo "graphbench-agent: arm $ARM has no allowlist (A0 and A5 are the two that exist today)" >&2; exit 2 ;;
esac

export GRAPHBENCH_QUESTIONS="$QUESTIONS"
export GRAPHBENCH_CATEGORY="$CATEGORY"
export GRAPHBENCH_PROJECT="$PROJECT"
export GRAPHBENCH_LIMIT="$LIMIT"
mapfile -t SELECTED < <(python3 - <<'PY'
import json, os
questions = json.load(open(os.environ["GRAPHBENCH_QUESTIONS"]))
category = os.environ.get("GRAPHBENCH_CATEGORY", "")
project = os.environ.get("GRAPHBENCH_PROJECT", "")
limit = int(os.environ.get("GRAPHBENCH_LIMIT", "3"))
picked = [q for q in questions
          if (not category or q["category"] == category)
          and (not project or q["project"] == project)]
for q in picked[:limit]:
    print(json.dumps(q))
PY
)

if [[ ${#SELECTED[@]} -eq 0 ]]; then
    echo "graphbench-agent: no question matched" >&2
    exit 2
fi

: > "$OUT"
echo "graphbench-agent: arm $ARM, model $MODEL, ${#SELECTED[@]} question(s)"
for line in "${SELECTED[@]}"; do
    id="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["id"])' "$line")"
    prompt_text="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["prompt"])' "$line")"
    root="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["root"])' "$line")"
    entry="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["entry"])' "$line")"

    read -r -d '' PROMPT <<EOF
You are answering one question about the Noeta project rooted at $root, whose entry file is $entry.
Every Noeta tool takes that entry path as its \`file\` argument.

Question: $prompt_text

Answer with ONE line of JSON and nothing else:
{"answer": ["<file>#<name>", ...]}

Each element names one declaration: the name of the file it is declared in, then '#', then the
declaration's own name. Order the list best first. An empty list is the right answer when nothing
satisfies the question.
EOF

    started=$(date +%s)
    # No pipe on the gated command: the JSON goes to a file and the exit code is read directly.
    claude --print \
        --output-format json \
        --model "$MODEL" \
        --max-turns "$MAX_TURNS" \
        --mcp-config "$WORK/mcp.json" \
        --strict-mcp-config \
        --add-dir "$root" \
        --allowedTools "$ALLOWED" \
        -- "$PROMPT" < /dev/null > "$WORK/raw.json" 2> "$WORK/raw.err"
    rc=$?
    elapsed=$(( $(date +%s) - started ))

    GRAPHBENCH_LINE="$line" GRAPHBENCH_RAW="$WORK/raw.json" GRAPHBENCH_RC="$rc" \
        GRAPHBENCH_ELAPSED="$elapsed" python3 - >> "$OUT" <<'PY'
import json, os, re, sys

question = json.loads(os.environ["GRAPHBENCH_LINE"])
rc = int(os.environ["GRAPHBENCH_RC"])
record = {
    "id": question["id"],
    "category": question["category"],
    "project": question["project"],
    "gold": question["gold"],
    "exit": rc,
    "seconds": int(os.environ["GRAPHBENCH_ELAPSED"]),
}
try:
    result = json.load(open(os.environ["GRAPHBENCH_RAW"]))
except Exception as problem:
    result = {}
    record["error"] = f"unreadable result: {problem}"

text = result.get("result") or ""
usage = result.get("usage") or {}
record["input_tokens"] = usage.get("input_tokens", 0)
record["output_tokens"] = usage.get("output_tokens", 0)
record["cost_usd"] = result.get("total_cost_usd", 0.0)
record["turns"] = result.get("num_turns", 0)

answer = []
match = re.search(r'\{.*"answer".*\}', text, re.S)
if match:
    try:
        answer = json.loads(match.group(0)).get("answer", [])
    except Exception:
        answer = []
record["answer"] = answer

def normalize(label):
    # The file's own name, not its path: an answer may spell a file relative to the project root, to
    # the repository, or not at all, and one project never has two files with the same name.
    label = label.strip().replace("\\", "/")
    head, _, name = label.rpartition("#")
    parts = [p for p in head.split("/") if p]
    return (parts[-1] if parts else "") + "#" + name.strip()

predicted = {normalize(a) for a in answer if "#" in a}
gold = {normalize(g) for g in question["gold"]}
hits = len(predicted & gold)
record["precision"] = 1.0 if not predicted and not gold else (hits / len(predicted) if predicted else 0.0)
record["recall"] = 1.0 if not gold and not predicted else (hits / len(gold) if gold else 0.0)
p, r = record["precision"], record["recall"]
record["f1"] = 0.0 if p + r == 0 else 2 * p * r / (p + r)
print(json.dumps(record))
PY

    # The row travels as an argument, not on stdin: a `python3 - <<'PY'` here would take the
    # heredoc as stdin and read nothing from the pipe.
    GRAPHBENCH_ROW="$(tail -1 "$OUT")" python3 - <<'PY'
import json, os
row = json.loads(os.environ["GRAPHBENCH_ROW"])
note = "  " + row["error"] if row.get("error") else ""
print("  {id:<34} F1={f1:.3f}  turns={turns}  tokens={i}/{o}  ${cost:.4f}  {sec}s{note}".format(
    id=row["id"], f1=row["f1"], turns=row["turns"], i=row["input_tokens"],
    o=row["output_tokens"], cost=row["cost_usd"], sec=row["seconds"], note=note))
PY
done

echo
GRAPHBENCH_OUT="$OUT" python3 - <<'PY'
import json, os
rows = [json.loads(line) for line in open(os.environ["GRAPHBENCH_OUT"]) if line.strip()]
if not rows:
    raise SystemExit("graphbench-agent: no answers")
n = len(rows)
mean = lambda key: sum(r[key] for r in rows) / n
print(f"graphbench-agent: {n} question(s)")
print(f"  F1 {mean('f1'):.3f}   precision {mean('precision'):.3f}   recall {mean('recall'):.3f}")
print(f"  turns {mean('turns'):.1f}   input tokens {mean('input_tokens'):.0f}   "
      f"output tokens {mean('output_tokens'):.0f}")
print(f"  cost ${sum(r['cost_usd'] for r in rows):.4f} total, ${mean('cost_usd'):.4f} per question")
print(f"  answers in {os.environ['GRAPHBENCH_OUT']}")
PY
