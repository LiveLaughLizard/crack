#!/usr/bin/env bash
# Runs crack's coding evals: each task copies a fixture repo to a temporary folder,
# asks crack (headless) to do the task, then checks the result with the fixture's own
# tests. Results are appended to evals/results/<date>.jsonl and summarized.
#
#   evals/run.sh                              # all tasks against the built-in mock
#   evals/run.sh --model anthropic/claude-sonnet-5-5 --max-cost 2
#   evals/run.sh --task rename-across-files --keep
#
# Needs: jq, git, cargo (rustup), and a built crack (`cargo build`; CRACK_BIN overrides).
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
model="custom/mock"
max_cost=""
only=""
keep=0
while [ $# -gt 0 ]; do
    case "$1" in
        --model) model="$2"; shift 2 ;;
        --max-cost) max_cost="$2"; shift 2 ;;
        --task) only="$2"; shift 2 ;;
        --keep) keep=1; shift ;;
        -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

crack="${CRACK_BIN:-$root/target/debug/crack}"
[ -x "$crack" ] || { echo "no crack binary at $crack (run cargo build)" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

work="$(mktemp -d "${TMPDIR:-/tmp}/crack-eval.XXXXXX")"
export CRACK_DATA_DIR="$work/data"
mock_pid=""
cleanup() {
    [ -n "$mock_pid" ] && kill "$mock_pid" 2>/dev/null
    [ "$keep" = 1 ] && echo "kept: $work" || rm -rf "$work"
}
trap cleanup EXIT

# The mock: scripted answers, no network, no cost. Real models use your own config.
mock=0
if [ "$model" = "custom/mock" ]; then
    mock=1
    mock_bin="${MOCK_BIN:-$root/target/debug/examples/mock_provider}"
    [ -x "$mock_bin" ] || { echo "no mock at $mock_bin (run cargo build --examples)" >&2; exit 2; }
    addr="${EVAL_MOCK_ADDR:-127.0.0.1:8975}"
    MOCK_ADDR="$addr" "$mock_bin" >/dev/null 2>&1 &
    mock_pid=$!
    export CRACK_BASE_URL="http://$addr/v1"
    export CRACK_CONFIG_DIR="$work/config"
    mkdir -p "$CRACK_CONFIG_DIR"
    sleep 0.3
fi

# Price per million tokens: input, output, cache read (US dollars). EVAL_PRICE="in,out"
# overrides; models not listed report no cost.
price_for() {
    if [ -n "${EVAL_PRICE:-}" ]; then echo "$EVAL_PRICE,"; return; fi
    case "${1#*/}" in
        custom/mock|mock) echo "0,0,0" ;;
        claude-fable-5*|claude-mythos-5*) echo "10,50,1" ;;
        claude-opus-5-5*) echo "4,20,0.2" ;;
        claude-opus-5*|claude-opus-4-[678]*) echo "5,25,0.5" ;;
        claude-sonnet-5-5*) echo "2,10,0.2" ;;
        claude-sonnet-5*) echo "2,10,0.2" ;;
        claude-sonnet-4-6*) echo "3,15,0.3" ;;
        claude-haiku-5-5*) echo "0.1,0.5,0.01" ;;
        claude-haiku-4-5*) echo "1,5,0.1" ;;
        *) echo "" ;;
    esac
}
prices="$(price_for "$model")"

mkdir -p "$root/evals/results"
results="$root/evals/results/$(date +%F).jsonl"
target="$root/evals/.cache/target"
spent=0
count=$(jq length "$root/evals/tasks.json")
ran=()

for i in $(seq 0 $((count - 1))); do
    task=$(jq -c ".[$i]" "$root/evals/tasks.json")
    name=$(jq -r .name <<<"$task")
    [ -n "$only" ] && [ "$only" != "$name" ] && continue
    if [ -n "$max_cost" ] && awk -v s="$spent" -v m="$max_cost" 'BEGIN { exit !(s >= m) }'; then
        echo "budget of \$$max_cost reached (\$$spent spent); skipping the rest" >&2
        break
    fi
    fixture=$(jq -r .fixture <<<"$task")
    steps=$(jq -r '.max_steps // 30' <<<"$task")
    if [ "$mock" = 1 ]; then prompt=$(jq -r .mock_prompt <<<"$task"); else prompt=$(jq -r .prompt <<<"$task"); fi

    dir="$work/$name"
    cp -r "$root/evals/fixtures/$fixture" "$dir"
    (cd "$dir" && git init -q && git add -A && git -c user.name=eval -c user.email=eval@localhost commit -qm fixture)

    start=$(date +%s.%N)
    out="$work/$name.json"
    timeout "${EVAL_TIMEOUT:-900}" "$crack" -C "$dir" run -m "$model" --format json \
        --permission-mode accept-edits --max-steps "$steps" "$prompt" </dev/null >"$out" 2>"$work/$name.err"
    run_status=$?
    seconds=$(awk -v s="$start" -v e="$(date +%s.%N)" 'BEGIN { printf "%.1f", e - s }')

    # The hidden tests arrive only now, so the model can't tailor its work to them.
    hidden=$(jq -r '.hidden // empty' <<<"$task")
    [ -n "$hidden" ] && cp -r "$root/evals/hidden/$hidden/." "$dir/"
    check=$(jq -r .check <<<"$task")
    if (cd "$dir" && CARGO_TARGET_DIR="$target" bash -c "$check") >"$work/$name.check" 2>&1; then pass=true; else pass=false; fi

    usage=$(jq -c 'select(.type=="done") | .usage' "$out" 2>/dev/null | tail -1)
    [ -z "$usage" ] && usage='{}'
    cost=$(jq -n --argjson u "$usage" --arg p "$prices" '
        if $p == "" then null else
            ($p | split(",") | map(select(. != "") | tonumber)) as $r
            | (($u.input_tokens // 0) + ($u.cache_write_tokens // 0)) * $r[0] / 1e6
              + ($u.output_tokens // 0) * $r[1] / 1e6
              + ($u.cache_read_tokens // 0) * ($r[2] // ($r[0] / 10)) / 1e6
        end')
    [ "$cost" != "null" ] && spent=$(awk -v s="$spent" -v c="$cost" 'BEGIN { printf "%.6f", s + c }')

    line=$(jq -nc --arg task "$name" --arg model "$model" --argjson pass "$pass" --argjson seconds "$seconds" \
        --argjson usage "$usage" --argjson cost "$cost" --argjson exit "$run_status" \
        '{date: (now | todate), task: $task, model: $model, pass: $pass, seconds: $seconds,
          input_tokens: ($usage.input_tokens // 0), output_tokens: ($usage.output_tokens // 0),
          cache_read_tokens: ($usage.cache_read_tokens // 0), cost: $cost, crack_exit: $exit}')
    echo "$line" >>"$results"
    ran+=("$line")
    printf '%-22s %s  %6ss\n' "$name" "$([ "$pass" = true ] && echo PASS || echo FAIL)" "$seconds"
    if [ "$pass" = false ]; then
        echo "  check output (last lines):"; tail -5 "$work/$name.check" | sed 's/^/    /'
        [ -s "$work/$name.err" ] && { echo "  crack stderr:"; tail -3 "$work/$name.err" | sed 's/^/    /'; }
    fi
done

[ ${#ran[@]} -eq 0 ] && { echo "no tasks ran" >&2; exit 1; }
printf '%s\n' "${ran[@]}" | jq -rs '
    "\n\(.[0].model): \(map(select(.pass)) | length)/\(length) passed, "
    + "\((map(.seconds) | add) * 10 | round / 10)s, "
    + "\(map(.input_tokens) | add) in / \(map(.output_tokens) | add) out tokens, "
    + (if any(.cost == null) then "cost unknown" else "$\((map(.cost) | add) * 10000 | round / 10000)" end)'
echo "results: $results"
printf '%s\n' "${ran[@]}" | jq -se 'all(.pass)' >/dev/null
