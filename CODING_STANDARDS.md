# Coding standards

Read by the `code-review` skill. When an agent does something you do not
want again, add a rule here instead of correcting it in chat. Rules that a
lint already enforces (`Cargo.toml` `[workspace.lints]`) do not belong here.

## Give errors a type

A library crate defines its own error enum per module boundary. It does not
return `Box<dyn Error>` or a bare `String`. The caller must be able to match
on what went wrong.

## Parse untrusted input at the boundary

Config files, CLI arguments, model output and tool-call arguments are
parsed into typed structs once, where they enter the system. Inner code
works with the typed value, never with raw strings or `serde_json::Value`.

## Make illegal states unrepresentable

Prefer an enum over a struct of `Option`s and flags that only make sense in
some combinations.

## Tests prove behaviour

Test through the public API of a crate. Derive expected values from the
requirement, not by repeating the implementation. A bug fix starts with a
test that reproduces the bug.

## Never log secrets or prompts

Diagnostics go through the `tracing` crate. Never log secrets, tokens or
full prompts that may contain user data.
