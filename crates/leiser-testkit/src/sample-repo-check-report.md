# repo-check: sample-repo @ 1a2b3c4d

Rust-Workspace mit einer Desktop-App; 120 Dateien, ein Mensch plus Agenten. Die Checks sind gut, die Bremsen fehlen.

## Must

1. **Verbote nur in Prosa, keine Bremse** (B1, B2)
   Beleg: Kein `.claude/settings.json`. `AGENTS.md:12` verbietet Pushes auf `main` nur als Text.
   Warum: Prosa ist keine Bremse.
   Fix: Getracktes `.claude/settings.json` mit `deny` für `git push origin main`.

2. **Kein Pre-Commit-Hook, Format-Reparatur-Commits** (D3)
   Beleg: Drei reine rustfmt-Commits seit letzter Woche; `core.hooksPath` ist leer.
   Warum: Zwischen Commit und CI läuft nichts.
   Fix: Getrackter Hook, der `cargo fmt --check` auf gestagten Dateien ausführt.

## Should

3. **Statusdateien im Repo** (H1, H2)
   Beleg: `docs/status-2026-11-01.md` und `docs/handoff.md`.
   Fix: Beide löschen; die History behält sie.

4. **Skill-Name doppelt (global)** (I1, D3)
   Beleg: `~/.claude/skills/review/` gegen `.agents/skills/review/`.
   Warum: Der persönliche Skill gewinnt.
   Fix: Den globalen Skill umbenennen.

5. **Lücken in `.gitignore`** (G2)
   Beleg: `git check-ignore` meldet `.claude/settings.local.json` als nicht ignoriert.
   Fix: Eine Zeile in `.gitignore`.

## Later

- Dateigrößen-Deckel (D7): zwei Dateien über 20 KB.
- Agent-Review (E1, E4): kein `code-review`-Skill.

## Nicht geprüft

- GitHub-Einstellungen (Branch-Schutz): mit `gh api` nachsehen.

## In Ordnung

- Checks: `cargo clippy` mit `-D warnings` in CI (D1, D2).
- Global: keine Konflikte (I1–I4).

Welche Nummern soll ich umsetzen?
