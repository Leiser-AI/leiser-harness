# 0001. PolyForm Noncommercial license

Status: accepted
Date: 2026-11-10

## Context

The source must be public, so GitHub Actions run free of charge and anyone
can read and build the code. Commercial use must not be allowed.

No OSI-approved open source license can forbid commercial use. That
restriction is exactly what disqualifies a license from being "open
source" in the OSI sense.

## Decision

The project uses the PolyForm Noncommercial License 1.0.0 (`LICENSE.md`).
The project describes itself as "source-available", not "open source".

## Consequences

- Private persons may use, change and share the code for noncommercial
  purposes.
- The license also allows use by charities, schools, public research
  organisations and government bodies. It is not limited to private
  persons.
- Commercial use needs a separate license from the copyright holder.
- Dependencies must use permissive licenses that allow this (see
  `deny.toml`). Copyleft dependencies such as GPL are not allowed.
- Outside contributions need a contributor license agreement (CLA) if the
  project wants to sell commercial licenses later.
- The repository being public does not depend on the license. Actions are
  free for every public repository.
