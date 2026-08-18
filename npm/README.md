# navin-engine

**Your agent writes the patch. This proves it.**

This package is a thin launcher for [`navin-engine`](https://github.com/navinspire-ai/navin-engine),
a Rust binary that boots your app in a throwaway git worktree, attacks it,
measures it, applies a candidate patch, measures again, and reports what
actually changed. It never edits your workspace.

The point of the npm package is that an MCP config becomes one line, with no
Rust toolchain to install:

```json
{
  "mcpServers": {
    "navin-engine": {
      "command": "npx",
      "args": ["-y", "navin-engine", "mcp"]
    }
  }
}
```

On first run the launcher downloads the release binary for your platform,
verifies its SHA-256 checksum, and caches it under
`~/.cache/navin-engine/<version>` (`%LOCALAPPDATA%` on Windows). Later runs
start instantly. Set `NAVIN_ENGINE_BIN` to use your own build instead, and
`NAVIN_ENGINE_HOME` to move the cache.

Full documentation, the tool reference and the per-environment wiring guide
(Cursor, Claude Code, Codex, Gemini CLI, OpenCode, Antigravity, Windsurf) are
in the [repository](https://github.com/navinspire-ai/navin-engine).

MIT licensed.
