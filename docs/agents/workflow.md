# Branches, pull requests and releases

- `development` is staging (beta builds), `main` is production (stable
  releases). Never commit to either.
- Each ticket gets its own short-lived branch from `origin/development`,
  named `<type>/<short-name>` (e.g. `feat/tool-registry`).
- One pull request per ticket, not per commit. Small follow-up changes
  (docs, glossary, config) go into the pull request that is already open.
  Write the body with `/pr`.
- Pull requests are squash-merged into `development`. Only the user merges.
  Auto-merge is allowed only once the automated agent review is a required
  check.
- A release is a pull request `development` -> `main`, merged with a merge
  commit, never squashed.
- A hotfix branches from `main` as `hotfix/<name>`. After the release it
  returns through a new branch `sync/main-<date>` from `main`, with a pull
  request to `development`.
- Pull requests are tested on Linux; macOS and Windows run on pushes to
  `development`, `main` and `hotfix/**`.
