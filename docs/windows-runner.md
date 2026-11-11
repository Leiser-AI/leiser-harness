# Own Windows runner

Pull requests run `cargo test --workspace` on the developer's Windows PC,
a GitHub Actions self-hosted runner with the label `leiser-windows`. GitHub's
own Windows runner still tests every push to `development`, `main` and
`hotfix/**`.

## Safety

The repository is public. Code from a pull request runs on that PC with the
runner's rights, so:

- The job runs only for pull requests whose branch is in this repository,
  never for pull requests from forks (`if:` in `.github/workflows/ci.yml`).
  Keep that condition.
- In the repository settings, under Actions > General > "Approval for
  running fork pull request workflows from contributors", keep "Require
  approval for all external contributors".
- Run the runner as its own Windows user without admin rights. Do not run it
  as the account that holds your browser, password manager or git
  credentials.

## Setup on the PC

1. Install the build tools, as that user:
   - Git for Windows (the tests run repository scripts with its `sh.exe`).
   - Visual Studio Build Tools with the "Desktop development with C++"
     workload (the MSVC linker).
   - rustup with the default `stable-x86_64-pc-windows-msvc` toolchain.
   - WebView2 Runtime (already part of Windows 11).
2. On GitHub open the repository's Settings > Actions > Runners > "New
   self-hosted runner", choose Windows x64 and run the commands it shows in
   PowerShell. They download the runner and register it with a one-time
   token. GitHub shows the token only there, so do not copy it anywhere else.
3. When `config.cmd` asks for labels, enter `leiser-windows`. Answer yes to
   running it as a service, so it starts with Windows.
4. The runner shows as "Idle" under Settings > Actions > Runners. The next
   pull request starts `test (own Windows runner)` on it.

## When the PC is off

The job waits in the queue until the runner is back, for at most 24 hours.
It is not a required check, so a pull request can still be merged without
it. The run after the merge on `development` then shows Windows failures on
GitHub's runner.
