# augur

Inline command suggestions for bash, one word at a time.

augur plugs into [ble.sh](https://github.com/akinomyoga/ble.sh)'s
auto-complete, the grey text after the cursor that `→` accepts, and replaces its
history source. ble.sh suggests the rest of the most recent command that starts
with what you typed. augur predicts only the word under the cursor, or the next
word once the current one is complete:

```
$ kubectl get po│ds            rest of the word, from your history
$ git checkout│ main           next word, once the current one is whole
$ dokcer│ [docker]             a command name that matches nothing, corrected
```

A whole-line suggestion is right only when you are repeating yourself. A word
can be right in a line you never typed before: in `kubectl -n staging get po`,
`pods` comes from what followed `get` in your other kubectl commands.

## What it suggests

The words come from two places. The shell's completion says what is valid here:
installed commands, subcommands, flags, files, and whatever the completion
scripts list, such as branches or pods. Your history says what you use, in this
directory, in this git repository, after the previous command, in this session.
When the history has no opinion, a local language model served by
[Ollama](https://ollama.com) picks among the valid words, or proposes the next
word when nothing else has one. The model is optional.

That the completion offers a word makes it valid, not likely, so augur is
careful about which ones it shows:

- Once a part of the word is typed, the completion's words that continue it are
  few, and one of them is suggested.
- With nothing typed they are every valid word. When there is only one, it is
  suggested. Among several, one shows only when the history or the model points
  at it. File names show only when the history does: ble.sh lists the directory
  for any command it has no completion for, and `kubectl get .git` helps nobody.
- A guess no completion backs, from the history or the model, shows only when it
  is likely enough, and never when it names a file that is not there. The
  `notes.txt` you read in another directory is not suggested in this one.
- A path guessed from the history comes one component at a time, `docs/` then
  `notes/` then `todo.md`, because the path you typed before is as often a
  sibling of the one you are typing as the path itself.
- A word never used with this command but used with another is suggested once
  three characters of it are typed: the file you read with `cat` is offered to
  `vim`.

augur prefers showing nothing to showing a guess it has no reason for.

## Requirements

- bash 5.0 or later with ble.sh 0.4
- Rust 1.85 or later, to build
- Linux. The engine watches `/proc` to exit with its shell; other systems are
  untested.
- Optional: Ollama, with a base model that returns log-probabilities. The
  default is `qwen2.5-coder:1.5b-base`.

## Install

```sh
git clone https://github.com/hvpaiva/augur
cd augur
cargo install --path .
augur import                          # one-time copy of ~/.bash_history
ollama pull qwen2.5-coder:1.5b-base   # optional: the language model
```

Then source the integration from `~/.config/blesh/init.sh` (or `~/.blerc`),
after anything that sets up completion:

```bash
source /path/to/augur/shell/augur.bash
```

Open a new shell and type. `ble-augur status` shows whether the engine is
running.

What the completion offers decides much of what augur suggests, so a command
without a completion script gets suggestions from the history and the model
only. For kubectl, for instance, add `source <(kubectl completion bash)` to
`~/.bashrc`.

## Options

Set with `bleopt name=value` after sourcing `shell/augur.bash`.

| option           | default | meaning                                                                 |
|------------------|---------|-------------------------------------------------------------------------|
| `augur_command`  | `augur` | the augur executable                                                    |
| `augur_timeout`  | `1000`  | milliseconds to wait for a suggestion                                   |
| `augur_model`    | empty   | Ollama model to ask; empty for the default, `none` to never ask         |
| `augur_fallback` | empty   | ble.sh sources to try when augur has nothing, e.g. `history syntax`     |
| `augur_ignore`   | empty   | colon-separated patterns of commands never recorded                     |
| `augur_log`      | empty   | file for the engine's diagnostics; by default one per shell, see status |

With fzf's completion integration (`ble-import integration/fzf-completion`), fzf
takes over the completion of git, tar, grep and some sixty other commands and
returns nothing while ble.sh auto-completes. augur runs their own completion
instead in that case. TAB keeps fzf's behaviour.

## Upgrading

The engine and the shell functions are installed separately, and a shell keeps
the functions it sourced for as long as it lives. After installing a new build,
run this in the shells that are open:

```sh
ble-augur restart
```

It sources `shell/augur.bash` again and restarts the engine. The two halves
state the version of the protocol they speak. When they differ augur stands
aside, ble.sh suggests as it does without augur, and `ble-augur status` says
what to do.

## Troubleshooting

`ble-augur status` is the first stop. It prints the engine state, the protocol,
the last problem if there was one, and the file holding the engine's
diagnostics.

| symptom                                      | cause and fix                                                                                        |
|----------------------------------------------|------------------------------------------------------------------------------------------------------|
| no suggestions at all after an upgrade       | the shell runs older functions than the engine: `ble-augur restart`                                  |
| `engine: disabled: ... speaks protocol N`    | the `augur` on `PATH` does not match `shell/augur.bash`: reinstall, then `ble-augur restart`         |
| `engine: disabled: the engine keeps exiting` | the reason follows the colon, often an unreadable state directory; the log named by status has more  |
| suggestions only after a long pause          | the model is slow to answer: `bleopt augur_model=none` rules it out                                  |
| file names where subcommands should be       | the command has no completion script, see [Install](#install)                                        |

To see why a line gets the suggestion it gets, outside the shell:

```sh
augur suggest 'git ch' --candidate checkout --candidate cherry
augur suggest 'cat Carg' --file Cargo.toml --file Cargo.lock
```

`--candidate` and `--file` stand for what the completion would offer.

## Measuring

`augur eval` replays your history. It types every command one character at a
time against models that only know the commands before it, and compares augur
with ble.sh's history source at the same positions:

```
measured 575 commands, 19512 characters

strategy         coverage  accuracy    useful     saved    latency
ble.sh history      45.3%     76.7%     34.8%     25.9%        1µs
augur               54.2%     82.6%     44.8%     32.9%        4µs

coverage  positions with a suggestion, measured after every typed character
accuracy  suggestions that were exactly the rest of the word or path component, or the next word
useful    positions with a right suggestion
saved     keystrokes saved accepting right suggestions with →
```

That is one person's history, so read it as an example of the report, not as a
benchmark. The replay has no completion to consult beyond command names, and it
cannot know which files existed when a command ran, so it measures the history
model more than augur in daily use. `--with-model --last 100` adds the language
model on the last 100 commands.

## Privacy

augur keeps a log of the commands you run, because that is what it learns from.
`~/.local/state/augur/history.tsv` holds what your shell history holds, plus the
directory, session, exit status and duration of each command. It is created
with mode 0600 and nothing sends it anywhere.

The language model runs on your machine. augur talks to it over
`http://127.0.0.1:11434`, ignoring any proxy in the environment, and asks it by
default: `bleopt augur_model=none` turns that off. A prompt contains the
directory, the line being typed and up to twelve recent commands. Lines that
mention `password`, `token`, `secret` and the like are left out, and the model
is not asked about a line being typed that mentions them. That filter is a
keyword list: a credential typed as a bare argument passes it, and lands in the
log as it lands in `~/.bash_history`.

Commands run under `set +o history`, starting with a space (with
`HISTCONTROL=ignorespace` or `ignoreboth`), matching `HISTIGNORE`, or matching
`bleopt augur_ignore` are not recorded. `augur import` copies `~/.bash_history`
as it is. Suggestions never contain control characters, blanks other than the
space, or invisible Unicode formatting. [SECURITY.md](SECURITY.md) has the rest.

## How it works

`shell/augur.bash` records every executed command and registers augur as the
first ble.sh auto-complete source. On each pause in typing it collects ble.sh's
completion candidates for the word under the cursor and asks `augur serve`, one
background process per shell, which word comes next. The engine answers only
the newest query, since the shell abandons a query as soon as you type again.
Questions for the model run on their own thread, with a timeout.

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) describes the pieces, the decision
rules and the protocol. [CONTRIBUTING.md](CONTRIBUTING.md) covers building and
testing.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. Unless you state otherwise, any
contribution you submit for inclusion in this work, as defined in the Apache-2.0
license, is dual licensed as above, without additional terms.
