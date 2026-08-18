# Changelog

## Unreleased

### Added

- Every `fix`, `optimize` and promotion write a markdown sibling next to
  the JSON (same stem, `.md`). MCP replies point at it as `report_md`, so
  an agent opens a page instead of parsing the artefact.
- Every candidate now carries the unified `diff` of what it changed, rejected
  ones included, captured with git inside the shadow before it is destroyed.
  A measurement can be reviewed instead of believed.
- `navin-engine pr --id <promotion>`: push the promotion branch and open a
  pull request whose body is the measured evidence. Uses the GitHub CLI when
  it is installed and authenticated; otherwise the branch is still pushed and
  a compare link is returned, so no token is ever stored.
- `navin-engine mcp`: the engine as an MCP server on stdio, so any AI coding
  environment can drive it. Nine tools (`inspect_project`, `prove`,
  `diagnose`, `fix`, `optimize`, `evolve`, `promotions`,
  `open_pull_request`, `verify_certificate`), progress notifications for long
  runs, and candidate patches passed inline so the host's own model is the
  generator.
- An npm launcher (`npx -y navin-engine mcp`), so an MCP config is one line
  and no Rust toolchain is needed. It downloads the release binary for the
  platform once, verifies its SHA-256, and caches it.
- `README.md`, `docs/hosts.md`, `docs/mcp.md` and a MIT `LICENSE` for
  standalone use, plus an animated demo built from a real optimize run rather
  than a mockup.

### Fixed

- A campaign that was cancelled or crashed left its shadow behind, and every
  later campaign failed with `shadow opt-base already exists` until the daemon
  restarted. Leftovers are now reclaimed and rebuilt from clean code.

## 0.1.0

First engine: shadow worktree isolation, automatic detection of start, test
and URL, fault-injection proofs with `quick`/`standard`/`deep` profiles,
diagnosis into actionable findings, a measured fix gate, statistical
optimization runs with differential behaviour checks, Ed25519-signed
promotion certificates, and a job daemon with a versioned local IPC.
