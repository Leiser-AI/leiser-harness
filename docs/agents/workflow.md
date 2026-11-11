# Branches, pull requests and releases

- `development` is staging (beta builds), `main` is production (stable
  releases). Never commit to either.
- Each ticket gets its own short-lived branch from `origin/development`,
  named `<type>/<short-name>` (e.g. `feat/tool-registry`).
- One pull request per ticket, not per commit. Small follow-up changes
  (docs, glossary, config) go into the pull request that is already open.
  Write the body with `/pr` and put `Closes #<ticket>` in the body: a squash
  merge takes its message from the body, so a keyword only in a commit is
  lost.
- Before you open a pull request, review your own diff with the
  `code-review` skill against `origin/development` and fix every finding,
  whatever its level. When a finding is one a tool can catch (a lint, a
  test, a CI step), add that check instead of relying on review next time.
  There is no review in CI: CI runs only deterministic checks.
- Pull requests are squash-merged into `development`. Only the user merges.
- Turn on Auto-fix for every pull request you open, so conflicts and CI
  failures reach you without the user relaying them. Resolve a conflict by
  merging `origin/development` into your branch and pushing normally; never
  force-push.
- A release is a pull request `development` -> `main`, merged with a merge
  commit, never squashed.
- A hotfix branches from `main` as `hotfix/<name>`. After the release it
  returns through a new branch `sync/main-<date>` from `main`, with a pull
  request to `development`.
- Pull requests and pushes test on GitHub's Linux, macOS and Windows
  runners (free for this public repository).
