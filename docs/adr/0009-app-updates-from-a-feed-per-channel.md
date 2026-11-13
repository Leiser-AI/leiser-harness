# 0009. The app updates itself from a feed per release channel

Status: accepted
Date: 2026-11-12

The desktop app updates itself with Tauri's updater. It looks for a new
version when it starts, every ten minutes while it runs and when its
window gets the focus again, shows a found one in the sidebar, and on a
click downloads it, checks its signature, installs it and restarts. The
harness service is not stopped: the installer replaces its
program next to the app, and the service moves to it once no turn runs
(ADR 0007). On Windows the installer renames the running service program
first, since Windows cannot overwrite a running program.

There are two channels: stable (versions `X.Y.Z` from `main`) and beta
(versions `X.Y.Z-beta.N` from `development`). A build reads the feed of
its own channel, fixed when it is built:
`https://raw.githubusercontent.com/Leiser-AI/leiser-harness/update-feed/<channel>.json`.
The branch `update-feed` holds only these files. A published release
writes its manifest there: a beta to `beta.json`, a stable release to both,
so beta users get stable releases too; the version decides, not GitHub's
pre-release flag. Drafts are never served.

Amended 2026-11-12: the README links to the newest installers under fixed
names at `releases/latest/download/`, which GitHub resolves to the latest
release that is not a pre-release. So a beta is published as a pre-release
only once a stable release exists; until then it is the latest release.

Updates are signed with a key of the maintainer; the public key is in
`tauri.conf.json`, the private key and its password are GitHub secrets.
Installed apps accept only updates signed with that key: losing it means
they cannot update anymore, and the maintainer keeps a copy outside GitHub.

The updater uses the operating system's TLS (`native-tls`), not rustls:
rustls brings Mozilla's root certificates under CDLA-Permissive-2.0, which
`deny.toml` does not allow.

## Considered options

- Feed at `releases/latest/download/latest.json`: GitHub's "latest" skips
  pre-releases, so there is no URL for the beta channel.
- Feed as an asset of a fixed release: that release would show on the
  releases page, where people look for installers.
- MSI installers: they accept only numeric versions, not `X.Y.Z-beta.N`,
  and the updater installs the NSIS installer on Windows anyway. Windows
  gets only the NSIS installer.
- AppImage on Linux: the harness service outlives the app, and an
  AppImage's files are gone once it quits. The updater installs the `.deb`
  and `.rpm` instead.

## Consequences

The feed URL and the public key are built into every installed app:
moving the feed or changing the key needs a release that still reads the
old feed and is signed with the old key.
