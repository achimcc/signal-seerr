# CLAUDE.md

Working rules for this repository. See `README.md` for what the bot does and
how to run it.

## Language

**This repository is English** — identifiers, comments, commit messages,
README. This is an exception to its author's usual habit of writing in
German: a tool that other people could use is unusable for most of them in
German.

The **messages the bot sends** are bilingual. User-facing strings live in
`i18n/*.toml`, never in the source. Which language somebody gets comes from
their Authentik locale — there is no language command.

## The test cycle

1. **Write the test first**, see it red for the reason you expect.
2. **Implement the smallest thing that passes.**
3. **Run all three, every time**: `cargo test`,
   `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`. A green
   test suite with a skipped clippy or fmt pass is not a green build — it
   only looks like one until the next CI run or the next person's `cargo
   fmt`.
4. **Commit**, staging the files by name — never `git add -A`.

Changed `nix/module.nix` or `nix/test.nix`? Also run
`nix build .#checks.x86_64-linux.vm --no-link` before calling it done. It
builds a system closure and boots a VM, so it is the slow one — but a module
change that only `nix flake check`'s cheaper checks have seen has not been
tested at all, only evaluated.

**Ask for the result, not the exit code.** `curl -w '%{http_code}'` and a
`202 Accepted` both look like success and are not proof of anything; the
webhook test in `nix/test.nix` asserts the actual HTTP status a wrong token
gets refused with, not just that the server answered.

## Rules that hold everywhere

- **No secret in `settings`.** The NixOS module's `settings` option becomes a
  world-readable file in the Nix store; every credential is a `*_file`
  option pointing somewhere else instead. `config.rs` has no field for a
  secret in plain text, on purpose — only paths.
- **The webhook token is checked before the body is parsed**, and compared
  with `Secret::matches`, not `==` — a plain comparison returns at the first
  differing byte and leaks the token's length and prefix to anyone who can
  time the answer.
- **A malformed or unrecognised webhook payload is never a 4xx that makes
  Seerr retry forever.** Only an authentication failure is; everything
  Seerr might legitimately send that this bot does not act on is accepted
  and dropped.
- **The webhook payload's `requestedBy_username` is not a username** — it is
  Seerr's `displayName`, editable by the person themselves. Identity comes
  from `jellyfinUsername` instead, matched against Authentik's username, the
  same field `user_id()` uses. One identity source, not two; see
  `src/seerr/mod.rs`.
- **`Type = simple`, never `oneshot`, and a bounded `StartLimitBurst`.** Both
  are answers to a real fault in a sibling project: a `oneshot` that had not
  finished, and separately an unbounded restart loop, each left a container
  stuck in `activating` until a deploy aborted on it. See `nix/module.nix`.
- **A test that pins exact user-facing wording is a trap for the next
  wording change.** Prefer asserting structure (a placeholder got filled, a
  status code, a set of keys) over the literal sentence, except where the
  literal sentence is the thing under test.
- **Versions are not guessed.** `cargo add <crate>` decides them,
  `Cargo.lock` holds them, and the Nix package reads
  `cargoLock.lockFile` — never a hash pinned by hand.

## Layout

| Path | Responsibility |
|---|---|
| `src/main.rs` | startup: load config and secrets, wait for signal-cli's socket, wire everything together |
| `src/config.rs` | `Config`, `Secrets` — parsing and checking the TOML, reading the `*_file` secrets |
| `src/secret.rs` | `Secret` — a value that never prints and compares in constant time |
| `src/model.rs` | shared value types (`Aci`, and the like) with no behaviour of their own |
| `src/signal/` | the signal-cli JSON-RPC client: send, resolve a username, become one |
| `src/seerr/` | the Seerr REST client: search, request, withdraw, look up a requester |
| `src/directory/` | the Authentik reconciler — diffing the directory against known state, four transitions (`diff.rs`) |
| `src/dialog/` | the conversation state machine: search results, season questions, commands |
| `src/webhook.rs` | the Seerr webhook listener — `MEDIA_AVAILABLE` / `MEDIA_FAILED` back to the requester |
| `src/state.rs` | the on-disk mapping between Signal accounts and Authentik usernames |
| `src/i18n.rs`, `i18n/` | locale selection and the message catalogues |
| `nix/module.nix`, `nix/test.nix` | the NixOS module and its VM test |

`src/lib.rs` re-exports these as a library so `tests/` can drive them without
going through `main`.
