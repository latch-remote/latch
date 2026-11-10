<!-- Thanks for the pull request. Keep what applies and delete the rest. -->

## What this changes

<!-- What a person using Latch will notice, then how it is done. Link the issue it closes. -->

## How it was checked

<!-- Commands run, machines and GPUs it ran on, numbers before and after for anything about performance. Say plainly what could not be tested: "not run on Windows hardware" is a fine answer. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] UI changes: snapshots regenerated and looked at (`cargo test -p latch-client -p latch-host snapshots -- --ignored`)
- [ ] Wire or control-API changes: older hosts and viewers still work, and `docs/PROTOCOL.md` says what is new
- [ ] `CHANGELOG.md` has a line under `## Unreleased` (add the heading if it is not there) for anything a user would notice
