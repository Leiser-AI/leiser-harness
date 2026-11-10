# leiser-harness

An agent harness written in Rust.

> **Source-available, not open source.** Free for private, noncommercial
> use under the [PolyForm Noncommercial License 1.0.0](LICENSE.md).
> Commercial use requires a separate license. See
> [ADR 0001](docs/adr/0001-polyform-noncommercial-license.md).

## Development

Requires a stable Rust toolchain (`rustup` picks it up from
`rust-toolchain.toml`).

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same checks on Linux, macOS and Windows, plus `cargo deny`
(vulnerabilities and licenses) and a secret scan.

## Working with coding agents

- `AGENTS.md` – rules and checks for every agent (`CLAUDE.md` imports it).
- `CODING_STANDARDS.md` – standards the `code-review` skill enforces.
- `GLOSSARY.md` and `docs/adr/` – domain language and architecture
  decisions. Created by `/grill-with-docs` when a term or decision is
  actually settled, not up front.
- `.agents/skills/` – the `engineering` and `productivity` skills from
  [mattpocock/skills](https://github.com/mattpocock/skills) (MIT, see
  `.agents/skills/LICENSE`), installed with the
  [skills](https://skills.sh) CLI and pinned in `skills-lock.json`.
  `.claude/skills/` links to them. Start with `/ask-matt` if you are unsure
  which one fits. Update with `npx skills@latest update` and review the
  diff before you commit it.
