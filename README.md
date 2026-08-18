# navin-engine

**Your agent writes the patch. This proves it.**

An AI coding agent can produce a plausible change in seconds. Nothing in that
loop tells you whether the service still survives a restart under load, or
whether the "optimization" actually made anything faster. `navin-engine` is
the missing half: it boots your app in a throwaway git worktree, attacks it,
measures it, applies a candidate patch, measures again, and reports what
changed with numbers instead of adjectives.

It never edits your workspace. Every experiment happens in a shadow copy, and
every artefact lands under `.navin/`.

```
$ navin-engine proof
{ "verdict": "fail", "robustness_score": 40,
  "faults": [ { "fault": "kill_recovery", "verdict": "fail", ... } ] }
```

- **Zero configuration.** Start command, test command and local URL are
  detected. The URL is not guessed: the app is booted once and watched until
  it opens a port.
- **No model, no API key.** The engine measures and gates; where candidate
  patches come from is your choice. In an AI coding tool, they come from the
  model you are already talking to.
- **Runs anywhere MCP runs.** Cursor, Claude Code, Codex, Gemini CLI,
  OpenCode, Antigravity, Windsurf, or plain shell in CI.

## Install

```bash
# From source (needs a Rust toolchain)
git clone https://github.com/navinspire-ai/navin-engine
cd navin-engine && cargo build --release
# the binary is at target/release/navin-engine
```

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/navinspire-ai/navin-engine/releases). Put one on
your `PATH` and you are done: the engine is a single self-contained
executable with no runtime dependency beyond `git`.

## Use it from your AI coding tool

Register the engine as an MCP server. In Cursor, `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "navin-engine": {
      "command": "navin-engine",
      "args": ["mcp"]
    }
  }
}
```

In Claude Code, one command:

```bash
claude mcp add navin-engine -- navin-engine mcp
```

Every other environment (Codex, Gemini CLI, OpenCode, Antigravity, Windsurf,
Zed, CI) is covered in [docs/hosts.md](docs/hosts.md), including the timeout
settings that matter, because a proof takes minutes rather than seconds.

### The loop your agent follows

1. `diagnose` - the engine boots the app, breaks it (load, restart,
   dependency loss), and returns findings with stable ids, a symptom, a root
   cause and a remediation direction.
2. Your agent writes patches for one finding. Independent alternatives, not
   steps of one change.
3. `fix` - each candidate is applied in its own shadow worktree, re-proved,
   run against your test suite, and accepted only if it resolves the finding
   without regressing anything else.
4. `optimize` - same idea for speed: a baseline, then each variant under
   identical load, repeated to estimate noise. A variant wins only if it
   beats the baseline by a margin larger than the noise, keeps the tests
   green, and answers byte-identically on replayed traffic.

The engine reports evidence; your agent applies the winner with its own edit
tools. Here is what step 4 looks like on the wire:

```json
{ "method": "tools/call",
  "params": { "name": "optimize",
    "arguments": { "candidates": [ {
      "id": "drop-fixed-sleep",
      "rationale": "the handler sleeps 50 ms on every request for no reason",
      "patch": { "kind": "files",
                 "edits": [ { "path": "app.py", "contents": "..." } ] } } ] } } }
```

and what comes back:

```json
{ "baseline": { "p95_ms": 54.8, "rps": 150.0, "robustness_score": 100 },
  "variants": [ { "candidate": "drop-fixed-sleep", "gain_percent": 93.5,
                  "significant": true, "behavior_equivalent": true,
                  "note": "P95 3.5 ± 0.1 ms, 2083.0 ± 14.1 RPS over 2 windows" } ],
  "winner": "drop-fixed-sleep", "winner_gain_percent": 93.5 }
```

### Tools

| Tool | What it does |
| --- | --- |
| `inspect_project` | How the project builds, tests and starts, per unit. Cheap, read-only. |
| `prove` | Fault injection against the running app; verdict plus a robustness score. |
| `diagnose` | A proof, explained: findings with ids you can act on. |
| `fix` | Verify your candidate patches against one finding. |
| `optimize` | Benchmark performance variants against the unmodified code. |
| `evolve` | Autopilot, for projects that configure their own candidate generator. |
| `promotions` | Changes the engine accepted, each with a signed certificate. |
| `verify_certificate` | Re-check a promotion's gate, checksum and signature. |

## Use it from the shell

Every tool is also a subcommand, printing JSON on stdout and logs on stderr:

```bash
navin-engine inspect                 # what the engine detected
navin-engine proof --profile deep    # robustness verdict
navin-engine diagnose                # findings
navin-engine fix --finding crash.load --candidates patches.json
navin-engine optimize --objective p95 --candidates variants.json
navin-engine promotions
```

`--start`, `--url` and `--test` exist for the rare case where detection picks
the wrong program in a monorepo. You should not need them.

## How it works

**Shadow isolation.** Each run gets a git worktree under `.navin/evolve/`,
with installed dependencies (`node_modules`, `.venv`, `vendor`) lent by
symlink so the app starts as it does in your checkout. Nothing outside that
worktree is written, and it is destroyed afterwards.

**Detection.** A `Procfile`, unit scripts (`package.json`, `pyproject.toml`,
`Cargo.toml`, `go.mod`, `pom.xml`, `composer.json`, ...) and a `Makefile` are
read in that order, and every plausible start command is tried until one
opens a port. The port is observed from the OS, matched to the process group
the engine spawned, so a monorepo with several servers cannot confuse it.

**Proof.** Profiles `quick`, `standard` and `deep` inject progressively more
faults: sustained load, process kill and recovery, dependency loss, resource
pressure. Each fault checks invariants (did it survive, did it recover, did
the error rate stay bounded) and the report is only as strong as its weakest
check.

**Gate.** A candidate is accepted when it resolves the target finding, adds
no new high-severity finding, keeps the test suite and your declared business
invariants green, and does not regress latency. Every decision carries its
reasons.

**Certificates.** An accepted change is committed on its own branch with an
Ed25519-signed certificate of the measurements that justified it, so a
promotion can be re-verified later by anyone.

## Configuration

Everything above works with no configuration. `.navin/evolve.toml` is there
for the parts only you can decide:

```toml
[evolve]
enabled = false        # true lets the engine open branches for accepted changes

[[invariants]]         # business truths that must survive every patch
name = "checkout works"
command = "npm run test:checkout"

[evolve.generator]     # optional: your own candidate generator
command = ""           # a program reading a finding on stdin, writing candidates on stdout
```

With `enabled = false` (the default) the engine measures and proposes,
and never touches your branches.

## Artefacts

```
.navin/
  proofs/<commit>.json        robustness reports
  diagnoses/<commit>.json     findings
  fixes/<commit>.json         candidate attempts and gate decisions
  optimize/<commit>.json      benchmark runs
  promotions/<id>.json        accepted changes and certificates
  evolve/                     shadow worktrees, daemon socket, state
```

All of it is JSON with a `schema` field. Add `.navin/` to your `.gitignore`,
or commit the reports if you want proof history in the repository.

## Requirements

- `git` (shadow worktrees are git worktrees)
- an app that listens on localhost; probes refuse any other host
- Linux, macOS or Windows

## License

MIT. See [LICENSE](LICENSE).
