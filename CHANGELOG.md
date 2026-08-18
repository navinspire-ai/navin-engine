# Changelog

## Unreleased

### Added

- `navin-engine mcp`: the engine as an MCP server on stdio, so any AI coding
  environment can drive it. Eight tools (`inspect_project`, `prove`,
  `diagnose`, `fix`, `optimize`, `evolve`, `promotions`,
  `verify_certificate`), progress notifications for long runs, and candidate
  patches passed inline so the host's own model is the generator.
- An npm launcher (`npx -y navin-engine mcp`), so an MCP config is one line
  and no Rust toolchain is needed. It downloads the release binary for the
  platform once, verifies its SHA-256, and caches it.
- `README.md`, `docs/hosts.md` and a MIT `LICENSE` for standalone use.

## 0.1.0

First engine: shadow worktree isolation, automatic detection of start, test
and URL, fault-injection proofs with `quick`/`standard`/`deep` profiles,
diagnosis into actionable findings, a measured fix gate, statistical
optimization runs with differential behaviour checks, Ed25519-signed
promotion certificates, and a job daemon with a versioned local IPC.
