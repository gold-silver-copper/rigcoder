# The improvement harness

Runs rigcoder against Terminal-Bench through [Harbor](https://github.com/harbor-framework/harbor)
and lets rigcoder edit itself between generations.

## Pieces

- `rigcoder_agent.py`: a Harbor `BaseInstalledAgent`. `install` uploads the
  Linux `rigcoder` binary into the task container; `run` executes it in `/app`
  on the task instruction with the transcript written to `/logs/agent/`, which
  Harbor copies into the trial directory.
- `build-linux.sh`: cross-builds that binary in a `rust:1.95` Docker container
  (cargo caches in named volumes), into `harness/bin/`.
- `iterate.py`: the loop. Build, evaluate, keep-or-revert, improve, repeat.
  The improve step runs the host `rigcoder` on this repository with a
  meta-task and a report of the failed trials; it may only edit the files in
  `MUTABLE` (the prompt, the tools, the agent settings, the transcript shaping,
  the CLI), and a change that does not `cargo check` is reverted.
  Kept generations are commits on the `evolve` branch; `ledger.jsonl` records
  every generation's score and rewards.

## First run

```sh
uv tool install harbor
export ANTHROPIC_API_KEY=...
./harness/build-linux.sh
# baseline, no self-edit:
python3 harness/iterate.py --generations 1 --no-improve -i 'hello-*' -n 2
# then let it iterate:
python3 harness/iterate.py --generations 5 -i 'hello-*' -i 'fix-*' -n 4
```

`-i` globs select tasks from `terminal-bench@2.0`; run the full set by
omitting them. Results land in `harness/runs/<job>/`, with `report.md` and
`improvement.md` per generation.

## Status

Written against Harbor 0.22 (the API surveyed in `references/harbor`), not
yet executed end to end on this machine: the loop's Harbor invocation,
result parsing and revert logic need a first real run to confirm the trial
directory layout. The CLI it drives is smoke-tested locally.
