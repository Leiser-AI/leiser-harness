# Crate audit for #133

Base commit `48639a0` on `origin/development`. The inventory below covers the 134 tracked files under `crates/` after excluding both files named `git_host.rs`. Tests use temporary repositories, synthetic subscriptions, local child processes and loopback connections.

## Findings and regression evidence

| Finding | Correction | Regression test |
| --- | --- | --- |
| The MCP listener and accepted connections survived service shutdown. | The service owns the accept task, and the accept task owns its connection tasks. | `stopping_the_service_closes_its_mcp_connections_and_listener` |
| A full desktop notification queue blocked command replies on the same connection. | Reserve room for an events-lost notification and keep reading replies while the consumer catches up. | `a_slow_notification_consumer_can_still_execute_commands` |
| Cancelling a command left its reply waiter registered until a response or disconnect. The caller also owned the in-progress socket write. | Release the waiter when the command future drops. A connection-owned writer finishes each queued frame. | `cancelling_a_request_releases_its_reply_slot_without_a_service_reply` |
| A stale temporary endpoint file could supply the permissions for a freshly published IPC token on Unix. | Create a new owner-only file exclusively before publishing it with a rename. | `a_stale_temporary_file_cannot_change_endpoint_permissions` |
| Removing a host account or subscription could destroy files or credentials before its registry change was saved. Subscription renames and interchangeability changes also remained visible after a save failed. | Commit the prospective registry state first. Persist the cleanup intent alongside a removal, then delete the selected files and key. | `a_failed_removal_keeps_the_host_account_and_its_files`, `a_failed_save_preserves_subscription_settings_and_credentials` |
| An interrupted removal could leave credentials or files with no remaining registration. An older service could erase an unrecognized cleanup intent on its next save. | Replay outstanding cleanup at startup. Store the intent under version 5 while cleanup is pending. Resume writing version 4 after cleanup. Discard inconsistent intents that refer to active or future IDs. | `restart_finishes_interrupted_host_account_cleanup`, `inconsistent_cleanup_state_cannot_delete_a_registered_account` |
| A managed agent session inherited credentials for the other provider and an endpoint override for the selected provider. | Clear the listed provider credential variables and the selected provider's base URL before applying the subscription. | `a_login_subscription_runs_in_its_config_directory_without_other_sign_ins`, `managed_subscriptions_clear_inherited_credentials_for_both_drivers` |
| Login output could close while its process was still alive. Removing a registration or starting another login could race the previous process. | Publish the exit status independently of output backpressure, keep the output open through child exit, and wait for cancellation and pipe cleanup before removing the directory or launching the replacement login. | `login_output_stays_open_until_the_cancelled_child_is_reaped`, `a_completed_login_reports_its_outcome_before_output_is_drained` |
| A failed repo-check turn held its session until the watchdog expired. A running repo-check also lost its subscription if it was removed. | Finish immediately on the terminal failure event and track which subscription each active repo-check owns. | `a_failed_repo_check_turn_releases_the_session_and_reports_its_reason`, `a_running_repo_check_keeps_its_subscription` |
| Worktree cleanup removed uncommitted files when Git configuration hid untracked files or submodule changes from status. | Explicitly request both kinds of changes when deciding whether a merged worktree can be removed. | `untracked_work_is_kept_when_git_status_normally_hides_it`, `a_worktree_with_hidden_submodule_changes_is_kept` |
| Repository scripts accumulated both output streams in memory, and the truncation marker exceeded the 64 KiB limit. A running script also survived service shutdown. | Drain both pipes concurrently into bounded tails, include the marker in the UTF-8 output budget, and terminate the shell and its foreground workers before service shutdown completes. | `a_verbose_script_keeps_both_streams_within_its_output_budget`, `stopping_the_service_ends_a_running_setup_script` |
| A zero polling interval let startup report success before its command loop panicked. | Reject any zero interval before starting the service. | `zero_poll_intervals_fail_before_the_service_starts` |
| The fake agent accepted nested paths as session identifiers. | Parse the identifier at the CLI boundary before using it to locate a session file. | `a_session_id_must_be_one_file_name` |

## Evidence boundaries

The shutdown checks exercise an accepted HTTP connection and the actual login process and a foreground script worker holding an OS file lock. They do not establish that an arbitrary program which deliberately detaches its own grandchildren will exit. Storage regressions inject a failed replace or cleanup and restart the service. They establish process-restart behavior, without simulating physical power loss. The endpoint permission check covers Unix mode bits. The macOS desktop smoke test opened the Host accounts view against an isolated service profile.

The local interface deliberately reads whole service-to-client transcripts. This pass does not establish a total memory bound across all simultaneous clients. Git's clean-worktree check and its eventual removal are separate operations. The tests cover state present at the check. Live provider contract tests require signed-in accounts and are separate from the deterministic workspace suite.

## File inventory

Each row identifies the review surface. Fixture and asset rows record their role. A row without a finding is not a claim of runtime coverage for every input or interleaving.

### leiser-desktop (44 files)

| File | Surface |
| --- | --- |
| `.gitignore` | Generated output exclusions |
| `Cargo.toml` | Crate dependencies and platform gates |
| `README.md` | Desktop setup and lifecycle contracts |
| `build.rs` | Desktop build hook |
| `capabilities/default.json` | Window and command permissions |
| `icons/128x128.png` | Application icon asset |
| `icons/128x128@2x.png` | Application icon asset |
| `icons/32x32.png` | Application icon asset |
| `icons/icon.icns` | Application icon asset |
| `icons/icon.ico` | Application icon asset |
| `icons/icon.png` | Application icon asset |
| `icons/icon.svg` | Application icon asset |
| `package-lock.json` | Resolved frontend dependency inventory |
| `package.json` | Build entry points and package dependencies |
| `scripts/package.mjs` | Release archives, subprocesses and artifact paths |
| `scripts/update-manifest.mjs` | Release manifest construction and artifact selection |
| `src/app_update.rs` | Update verification and progress lifecycle |
| `src/bundle_swap.rs` | Archive staging, atomic bundle exchange and error cleanup |
| `src/connection.rs` | Service startup, reconnect and event forwarding |
| `src/main.rs` | Desktop application and command registration |
| `src/relaunch.rs` | Replacement service and app process lifetimes |
| `src/terminals.rs` | Terminal registry, writes and close callbacks |
| `tauri.conf.json` | Desktop runtime configuration |
| `tauri.release.conf.json` | Release configuration |
| `tsconfig.json` | TypeScript build configuration |
| `vite.config.ts` | Frontend build and local dev server |
| `web/index.html` | Desktop markup, forms and local resources |
| `web/src/appUpdate.ts` | Update UI lifecycle |
| `web/src/components.css` | Styles or build-time declarations |
| `web/src/diff.ts` | Diff rendering |
| `web/src/extensions.ts` | File read/write and reload state |
| `web/src/icons.ts` | Static icon definitions |
| `web/src/main.ts` | UI state, event ordering and rendering |
| `web/src/menu.ts` | Menu callback lifetime |
| `web/src/newChat.ts` | Chat startup and repository selection |
| `web/src/platform.ts` | Platform-dependent behavior |
| `web/src/protocol.ts` | Wire types |
| `web/src/service.ts` | Typed IPC calls and event subscriptions |
| `web/src/styles.css` | Styles or build-time declarations |
| `web/src/terminal.ts` | Terminal lifecycle and event handling |
| `web/src/tokens.css` | Styles or build-time declarations |
| `web/src/vite-env.d.ts` | Styles or build-time declarations |
| `web/src/wayfinder.ts` | Repository map rendering |
| `windows/installer-hooks.nsh` | Installer process and update lifecycle |

### leiser-harness-service (4 files)

| File | Surface |
| --- | --- |
| `Cargo.toml` | Crate dependencies and platform gates |
| `src/agents.rs` | Driver selection and subprocess configuration |
| `src/main.rs` | Service process, data-directory lock and update lifecycle |
| `tests/service_process.rs` | Regression suite for service process |

### leiser-service (34 files)

| File | Surface |
| --- | --- |
| `Cargo.toml` | Crate dependencies and platform gates |
| `src/checkpoint.rs` | Git refs, snapshot paths and restore ordering |
| `src/claude_code.rs` | Child lifecycle, input/output framing and managed environment |
| `src/cli_extensions.rs` | Discovery, symlink resolution and file replacement |
| `src/codex/conversation.rs` | Cancellation, turn events and request correlation |
| `src/codex/mod.rs` | Driver startup, process termination and managed environment |
| `src/codex/wire.rs` | Agent protocol deserialization |
| `src/driver.rs` | Credential selection, login cancellation and pipe lifetime |
| `src/git.rs` | Git subprocess invocation and output ownership |
| `src/git_host/github_poll.rs` | CI signals, log handling and polling boundaries |
| `src/git_host/github_wayfinder.rs` | GitHub response parsing and repository mapping |
| `src/handoff.rs` | Transcript ranges and resumed sessions |
| `src/hard_blocks.rs` | Tool argument classification |
| `src/harness_mcp.rs` | Token scope, request limits and connection ownership |
| `src/host_accounts.rs` | Host binding and registration directory ownership |
| `src/installed.rs` | Executable discovery, PATH handling and probes |
| `src/keychain.rs` | Credential storage, deletion and error handling |
| `src/lib.rs` | Public service API surface |
| `src/local_interface.rs` | Endpoint publication, framing, cancellation and flow control |
| `src/program.rs` | Service startup paths and child process ownership |
| `src/protocol.rs` | Typed service requests, responses and redaction |
| `src/repo_check.rs` | Read-only agent lifetime and terminal events |
| `src/repo_config.rs` | Configuration parsing and defaults |
| `src/script.rs` | Script process lifetime and stdout/stderr budgets |
| `src/service.rs` | Command sequencing, registration transactions and shutdown |
| `src/service/auto_fix.rs` | Polling state and serialized changes |
| `src/service/polls.rs` | Poll scheduling and pending operations |
| `src/service/pull_requests.rs` | Branch and worktree cleanup preconditions |
| `src/service/wayfinder.rs` | Background polling and result delivery |
| `src/store.rs` | Atomic replacement, version handling, transcript repair and cleanup replay |
| `src/subscriptions.rs` | Config and key ownership, shadow homes and registration changes |
| `src/todos.rs` | Persistent list updates |
| `src/version.rs` | Version parsing and comparisons |
| `src/worktree.rs` | Working directories, Git paths and branch operations |

### leiser-terminal (4 files)

| File | Surface |
| --- | --- |
| `Cargo.toml` | Crate dependencies and platform gates |
| `src/lib.rs` | PTY, reader and reaper lifetimes |
| `src/startup.rs` | PTY startup stream framing |
| `tests/terminal.rs` | Regression suite for terminal |

### leiser-testkit (48 files)

| File | Surface |
| --- | --- |
| `Cargo.toml` | Crate dependencies and platform gates |
| `src/bin/fake-agent-cli.rs` | Test CLI parsing, session files and input handling |
| `src/bin/fake-gh.rs` | Test Git host process arguments and fixture state |
| `src/fake_driver.rs` | Fake session setup, driver process and environment |
| `src/fake_gh.rs` | Local Git host fixtures |
| `src/gate.rs` | Deterministic test synchronization |
| `src/harness_mcp_client.rs` | Test HTTP framing and request/reply parsing |
| `src/keychain.rs` | In-memory test credential lifecycle |
| `src/lib.rs` | Public fixture API surface |
| `src/repo.rs` | Temporary Git repository lifecycle |
| `src/sample-repo-check-report.md` | Synthetic report fixture |
| `src/script.rs` | Deterministic agent script and step definitions |
| `src/service.rs` | Isolated test service and restart helpers |
| `src/turn.rs` | Turn event collection and deadlines |
| `tests/agent_cli_lookup.rs` | Regression suite for agent cli lookup |
| `tests/agent_cli_neighbours.rs` | Regression suite for agent cli neighbours |
| `tests/app_queries.rs` | Regression suite for app queries |
| `tests/auto_fix.rs` | Regression suite for auto fix |
| `tests/chat_settings.rs` | Regression suite for chat settings |
| `tests/checkpoints.rs` | Regression suite for checkpoints |
| `tests/claude_code_contract.rs` | Regression suite for claude code contract |
| `tests/claude_code_driver.rs` | Regression suite for claude code driver |
| `tests/claude_code_process.rs` | Regression suite for claude code process |
| `tests/cli_extensions.rs` | Regression suite for cli extensions |
| `tests/codex_contract.rs` | Regression suite for codex contract |
| `tests/codex_driver.rs` | Regression suite for codex driver |
| `tests/fixtures/data-v1/chats/2.jsonl` | Legacy on-disk migration fixture |
| `tests/fixtures/data-v1/state.json` | Legacy on-disk migration fixture |
| `tests/github_poll.rs` | Regression suite for github poll |
| `tests/github_wayfinder.rs` | Regression suite for github wayfinder |
| `tests/handoff.rs` | Regression suite for handoff |
| `tests/hard_blocks.rs` | Regression suite for hard blocks |
| `tests/host_accounts.rs` | Regression suite for host accounts |
| `tests/local_interface.rs` | Regression suite for local interface |
| `tests/login_shell_path.rs` | Regression suite for login shell path |
| `tests/permissions_and_approvals.rs` | Regression suite for permissions and approvals |
| `tests/persist_and_resume.rs` | Regression suite for persist and resume |
| `tests/pull_requests.rs` | Regression suite for pull requests |
| `tests/queue_steer_interrupt.rs` | Regression suite for queue steer interrupt |
| `tests/repo_check.rs` | Regression suite for repo check |
| `tests/repo_check_contract.rs` | Regression suite for repo check contract |
| `tests/service_update.rs` | Regression suite for service update |
| `tests/subscriptions.rs` | Regression suite for subscriptions |
| `tests/todo_list.rs` | Regression suite for todo list |
| `tests/tracer_bullet.rs` | Regression suite for tracer bullet |
| `tests/usage_limits.rs` | Regression suite for usage limits |
| `tests/wayfinder.rs` | Regression suite for wayfinder |
| `tests/worktree_per_chat.rs` | Regression suite for worktree per chat |
