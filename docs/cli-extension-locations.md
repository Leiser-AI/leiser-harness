# Where agent CLIs load CLI extensions

The CLI extensions panel (#42) lists what each agent CLI loads from a
repository and edits it in the agent CLI's own files (ADR 0003). This page
records where Claude Code and Codex look, from their primary docs, as of
2026-11-11. The service code is `crates/leiser-service/src/cli_extensions.rs`.

## Repository level (listed by the panel)

| Kind | Claude Code | Codex |
| --- | --- | --- |
| Instruction files | `CLAUDE.md`, `.claude/CLAUDE.md`, `CLAUDE.local.md` at the root; `CLAUDE.md` / `CLAUDE.local.md` in subdirectories (loaded on demand); `.claude/rules/**/*.md`; files imported with `@path` (up to 4 hops, not inside code). `AGENTS.md` / `.claude/AGENTS.md` only when the root has no `CLAUDE.md`-family file. | In each directory from the root down to the working directory, the first of `AGENTS.override.md`, `AGENTS.md`. The panel lists every directory's, since agents follow nested `AGENTS.md` files too. |
| Skills | `.claude/skills/<name>/SKILL.md` | `.agents/skills/<name>/SKILL.md`, `.codex/skills/<name>/SKILL.md` |
| MCP servers | `.mcp.json`, key `mcpServers` | `.codex/config.toml`, tables `[mcp_servers.<name>]` (trusted projects only) |
| Hooks | `.claude/settings.json`, `.claude/settings.local.json`, key `hooks` | `.codex/hooks.json`, key `hooks`; inline `[[hooks.<Event>]]` in `.codex/config.toml` (trusted projects only) |

Both agent CLIs follow symlinked skill directories. The panel resolves
symlinks: a skill that `.claude/skills` links into `.agents/skills` is one
file, listed under both agent CLIs, and saving it keeps the link.

Nested instruction files are found with `git ls-files --cached --others
--exclude-standard`, so ignored directories such as `target/` are skipped.

## User level (not listed yet)

These depend on the subscription a chat runs on, because each subscription
has its own config directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).

- Claude Code: `~/.claude/CLAUDE.md`, `~/.claude/rules/`, `~/.claude/skills/`,
  hooks in `~/.claude/settings.json`, MCP servers in `~/.claude.json` (user
  scope at the top level, local scope under `projects["<abs path>"]`). All
  move under `$CLAUDE_CONFIG_DIR`, `.claude.json` included.
- Codex: `$CODEX_HOME/AGENTS.md` (or `AGENTS.override.md`),
  `$CODEX_HOME/config.toml` (`[mcp_servers]`, `[hooks]`),
  `$CODEX_HOME/hooks.json`, `$HOME/.agents/skills` (does not move with
  `CODEX_HOME`), `$CODEX_HOME/skills` (deprecated, still loaded).

## Sources

- Claude Code: <https://code.claude.com/docs/en/memory>,
  <https://code.claude.com/docs/en/skills>,
  <https://code.claude.com/docs/en/mcp>,
  <https://code.claude.com/docs/en/hooks>,
  <https://code.claude.com/docs/en/settings>,
  <https://code.claude.com/docs/en/claude-directory>
- Codex: <https://developers.openai.com/codex/guides/agents-md>,
  <https://developers.openai.com/codex/skills>,
  <https://developers.openai.com/codex/mcp>,
  <https://developers.openai.com/codex/hooks>,
  <https://developers.openai.com/codex/config-advanced>; source
  `openai/codex` `codex-rs/ext/skills/src/host_roots.rs`,
  `codex-rs/hooks/src/engine/discovery.rs`
