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

## Running on macOS with colima

Harbor's prebuilt task images are amd64-only; pass `--force-build` so the
environments are built natively from their Dockerfiles and the arm64 binary
matches. If the docker CLI config points at Docker Desktop's credential
helper, give harbor and the build script an empty config plus the colima
socket:

```sh
colima start pi --cpu 8 --memory 16
mkdir -p /tmp/dockercfg && echo '{"cliPluginsExtraDirs":["/opt/homebrew/lib/docker/cli-plugins"]}' > /tmp/dockercfg/config.json
export DOCKER_CONFIG=/tmp/dockercfg DOCKER_HOST=unix://$HOME/.colima/pi/docker.sock
export PYTHONPATH=$PWD RIGCODER_TIMEOUT_SECS=800
harbor run -d terminal-bench@2.0 -a harness.rigcoder_agent:RigcoderAgent \
  -m gemini/gemini-3.8-flash --ae GEMINI_API_KEY=$GEMINI_API_KEY \
  -i fix-git -i regex-log -n 4 --force-build -o harness/runs
```

## Baseline (2026-09-06)

Four tasks, one attempt each, reward and tool calls / wall time:

| task | gemini-3.1-pro-preview | gemini-3.8-flash (default) |
|---|---|---|
| fix-git | 1.0, 14 calls, 49 s | 1.0, 24 calls, 26 s |
| openssl-selfsigned-cert | 1.0, 22 calls, 93 s | 1.0, 50 calls, 137 s |
| sqlite-db-truncate | 1.0, 17 calls, 110 s | 1.0, 19 calls, 43 s |
| regex-log | 1.0, 22 calls, 213 s | 1.0, 55 calls, 218 s |

regex-log first scored 0: its bare `ubuntu:24.04` image has no CA store and
rig's default transport panicked instead of returning an error. The adapter
now installs `ca-certificates`, and the panic is fixed upstream in
[rig PR #2471](https://github.com/0xPlaygrounds/rig/pull/2471).

`iterate.py` (the self-edit loop) has not been run yet; the Harbor
invocation and result parsing it wraps are the ones above.
