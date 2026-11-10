# 0004. Tauri for the desktop app

Status: accepted
Date: 2026-11-10

The v1 desktop app (macOS and Windows) is built with Tauri: the core is
Rust, the user interface is TypeScript in a web view. The harness is meant
to be Rust, so this is the one place where we accept a second language. The
main screen is mostly rich, selectable text with markdown, a diff view and
a terminal, and the web ecosystem has mature components for all three.

## Considered options

- egui: pure Rust and fast, but lacks mature building blocks for rich
  selectable text, diffs and terminals; text input and accessibility are
  weaker.
- gpui (Zed's UI framework): pure Rust and proven for exactly these views,
  but its API is unstable and sparsely documented.

## Consequences

- The core does not depend on the UI, so the UI can be replaced later.
- The repository needs a TypeScript toolchain for the UI.
