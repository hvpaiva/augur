# Contributing

## Building and checking

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
shellcheck shell/augur.bash
```

All six must pass, and `cargo deny check` too when
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny) is installed.
`tests/serve.rs` drives the real `augur serve` binary over its protocol, a fake
Ollama included, so it covers the engine end to end. `tests/shell.rs` checks
that `shell/augur.bash` escapes text and states its protocol as the engine
does. Nothing automated covers the rest of the shell half: see
[Testing the shell half](#testing-the-shell-half).

## Layout

| path               | what it holds                                                  |
|--------------------|----------------------------------------------------------------|
| `shell/augur.bash` | recording hooks and the ble.sh auto-complete source            |
| `src/main.rs`      | the CLI: `serve`, `protocol`, `import`, `eval`, `suggest`      |
| `src/serve.rs`     | the engine's event loop                                        |
| `src/suggest.rs`   | the decision rules                                             |
| `src/tokens.rs`    | the history model                                              |
| `src/files.rs`     | which words name files, and whether they are there             |
| `src/protocol.rs`  | the line protocol and its version                              |
| `src/llm.rs`       | the Ollama client and the prompt                               |
| `src/eval.rs`      | the history replay behind `augur eval`                         |

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how they fit together.

## Changing the protocol

Any change to the messages in `src/protocol.rs` is a new protocol version. Bump
`VERSION` there and `_ble_augur_protocol` in `shell/augur.bash` in the same
commit. Shells stay open for days with the functions they sourced, and the
version is what turns a mismatch into a message instead of silence.

## Changing what gets suggested

Run `augur eval` before and after, on a history of a few hundred commands or
more, and put both tables in the pull request. A rule that raises coverage by
lowering accuracy is usually a loss: a wrong suggestion costs the user more
than a missing one.

New rules need a test in `src/suggest.rs` that names the behaviour, as
`never_guesses_a_file_that_is_not_there` does.

## Testing the shell half

Test a build before installing it, because `cargo install` replaces the engine
that your open shells start next. In a scratch shell:

```bash
bleopt augur_command=$PWD/target/release/augur
ble-augur restart
ble-augur status
```

Then type, and check at least these:

- a command with history, and one without
- an argument position with nothing typed, in a directory with files
- a file that your history knows from another directory
- `bleopt augur_command=/bin/false; ble-augur restart`: augur must disable
  itself with a reason and ble.sh must keep suggesting, with nothing printed
  over the prompt

`bleopt augur_log=/tmp/augur.log` collects the engine's diagnostics in one
place while you test.

## Style

Rust: `?` for errors, no `unwrap()` outside tests, `thiserror` in the library
and `anyhow` in the binary, `///` on public items. Bash: quote everything,
names under `ble/augur/` and `_ble_augur_`, ShellCheck clean, and a reason next
to every `shellcheck disable`.

Comments say what the code cannot: why a rule exists, what breaks without it.
Commit messages follow [Conventional Commits](https://www.conventionalcommits.org).
