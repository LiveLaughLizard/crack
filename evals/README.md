# Evals

Scripted coding tasks that check crack end to end: a model gets a small repository and a
request, and the repository's own tests decide whether it succeeded.

```sh
just eval                                              # every task, against the mock
just eval --task rename-across-files --keep            # one task; keep the temp folder
just eval-model anthropic/claude-sonnet-5-5 2          # a real model, stop after $2
```

Each task:

1. copies `fixtures/<fixture>` to a temporary folder and makes it a git repository;
2. runs `crack run --format json --permission-mode accept-edits --max-steps N` there, with
   its own data folder;
3. copies in `hidden/<name>` if the task has hidden tests (added only after the model is
   done, so it can't write to them);
4. runs the task's `check` command;
5. appends a line to `results/<date>.jsonl`: task, model, pass, seconds, tokens, cost.

The run ends with a summary per model: pass rate, time, tokens and cost. The exit status is
0 only if every task passed.

## Tasks

Tasks are in `tasks.json`:

| Task | Fixture | What it checks |
|---|---|---|
| `fix-failing-test` | `fix-test` | Finds and fixes a bug without touching the tests |
| `rename-across-files` | `rename` | Renames a function in four files; nothing still uses the old name |
| `add-feature` | `add-feature` | Adds `slugify`, checked by hidden tests |

Each task has a `prompt` for real models and a `mock_prompt` the mock server can act out
(`run: <command>`). Against the mock, the evals test crack's own pipeline: tools,
sandbox, permissions and the headless JSON. They don't measure model quality. To add a
task, add a fixture under `fixtures/` (Rust fixtures need a `rust-toolchain.toml`) and an
entry in `tasks.json`.

## Real models and cost

- **Config:** real models use your normal crack config and API keys. The mock gets an
  empty, isolated config.
- **Cost:** computed from the run's token usage with list prices for the Claude family in
  `run.sh`. Set `EVAL_PRICE="input,output"` (dollars per million tokens) for other models.
  Otherwise their cost shows as unknown.
- **Budget:** `--max-cost` (the `budget` argument) stops starting new tasks once that much
  has been spent. The task already running finishes.
- **Settings:** `EVAL_TIMEOUT` (seconds per task, default 900) and `EVAL_MOCK_ADDR`
  (default `127.0.0.1:8975`).
