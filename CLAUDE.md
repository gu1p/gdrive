# gdrive (gu1p fork) — working agreements

gdrive is a Rust CLI for Google Drive. This fork is based on dodomorandi/gdrive (google-drive3 7, hyper 1,
yup-oauth2 12, tokio). The ranked modernization plan is docs/triage/README.md; the supporting triage of every
upstream issue and PR is in docs/triage/. Read the plan section for your workstream before coding.

## Rules for agents working in this repository

- You work in a dedicated git worktree on your own branch. Commit there with conventional prefixes
  (feat:, fix:, refactor:, test:, ci:, docs:, build:). One logical change per commit. Never push, never
  rebase or reset shared history, never touch main or another worktree, never force anything.
- Before you report done: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`,
  `cargo build` all clean in your worktree. Fix what you broke; do not silence lints with allow attributes
  unless there is a comment explaining why.
- Keep the CLI backwards compatible with gdrive 3.9.x scripts: existing commands, flags, exit codes and
  stdout formats stay unless the plan explicitly changes them. New behavior goes behind new flags.
  Error messages start with `Error:` and keep the hints that tell the user which command to run next.
- Never call the real Google API from tests. Use in-process mock HTTP servers (hyper or a small test
  server) for protocol tests; unit tests live next to the code, integration tests under tests/.
- Match the surrounding code style: typed error enums with Display impls, tokio::fs and tokio::io for
  file I/O, no unwrap/expect on I/O or network results in non-test code, no new heavy dependencies
  without a reason stated in the commit message. MSRV stays what Cargo.toml says.
- Secrets never go into logs or output: no printing of tokens, refresh tokens, proxy URLs, session URLs.
- The worktree's untracked `.cargo/config.toml` limits build jobs so several agents can build at once.
  Do not commit it and do not raise the limit.
- Do not edit docs/triage/ (it is the record of the analysis); put user documentation under docs/ and README.md.
