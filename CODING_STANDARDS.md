# Coding standards

Read by the `code-review` skill. When an agent does something you do not
want again, add a rule here instead of correcting it in chat.

## Return errors, do not panic

Library code returns `Result`. It does not call `unwrap`, `expect`,
`panic!`, `todo!` or `unimplemented!`. Clippy warns on these.

In tests, `unwrap` and `expect` are fine. A binary's `main` may turn an
error into an exit code, in one place.

## Give errors a type

A library crate defines its own error enum per module boundary. It does not
return `Box<dyn Error>` or a bare `String`. The caller must be able to match
on what went wrong.

## Parse untrusted input at the boundary

Config files, CLI arguments, model output and tool-call arguments are
parsed into typed structs once, where they enter the system. Inner code
works with the typed value, never with raw strings or `serde_json::Value`.

## No `unsafe`

The workspace forbids `unsafe`. If you think you need it, stop and ask.

## Make illegal states unrepresentable

Prefer an enum over a struct of `Option`s and flags that only make sense in
some combinations.

## Tests prove behaviour

Test through the public API of a crate. Derive expected values from the
requirement, not by repeating the implementation. A bug fix starts with a
test that reproduces the bug.

## No output from libraries

Library crates do not print. Use the `tracing` crate for diagnostics, and
never log secrets or full prompts that may contain user data.
