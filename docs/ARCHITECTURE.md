# Architecture

augur has two halves that are installed separately: a bash file sourced into the
interactive shell, and a Rust engine that runs as one background process per
shell. They talk over a pair of FIFOs that ble.sh manages.

```
 interactive bash + ble.sh                         augur serve (one per shell)
┌──────────────────────────────┐                  ┌───────────────────────────────┐
│ shell/augur.bash             │   Q  query       │ serve.rs    event loop        │
│  PREEXEC/POSTEXEC hooks ─────┼──► history.tsv ─►│ history.rs  tails the log     │
│  auto-complete source:augur  │ ───────────────► │ suggest.rs  decision rules    │
│   collects ble.sh candidates │   R  response    │ tokens.rs   history model     │
│   shows the ghost text       │ ◄─────────────── │ files.rs    is the path there │
└──────────────────────────────┘                  │ llm.rs ───► Ollama (optional) │
                                                  └───────────────────────────────┘
```

## The shell half

`shell/augur.bash` does two independent things.

It records. ble.sh's `PREEXEC` hook notes the command, the directory and the
start time, and `POSTEXEC` appends one record to
`$XDG_STATE_HOME/augur/history.tsv`. The file is append-only and shared by every
shell, which is how a command run in one terminal is suggested in another a
moment later.

It suggests. `ble/complete/auto-complete/source:augur` is registered as the
first entry of `_ble_complete_auto_source`. ble.sh calls it on a pause in
typing. It runs ble.sh's own completion for the word under the cursor, tags
each candidate as a file name (`f`) or another word (`w`), sends the query, and
polls for the answer in 4 ms slices so that a keystroke cancels the wait. The
source returns 0 after showing a suggestion or deciding to show none, 148 when
the user typed meanwhile, and 1 when augur cannot answer, which lets ble.sh fall
back to its own sources.

The engine is started on the first idle moment and looked after by
`ble/augur/.start`. ble.sh can restart a background process by itself, but that
restart runs the `onstart` hook, which posts to the process, which restarts it
again: with an engine that exits as it starts, the recursion floods the terminal
with `Bad file descriptor`. augur therefore restarts the engine itself, at most
three times without an answer in between, and then disables itself with the last
line the engine wrote as the reason.

## The engine

`serve.rs` reads requests on one thread and sends them to the main loop over a
channel. Queries can arrive faster than they are answered, so each turn of the
loop drains the channel and answers only the newest query. Before planning, the
server polls the history log for records appended since the last query.

`Engine::plan` in `suggest.rs` returns either an answer or a question for the
language model together with a fallback. Questions run on their own thread. An
answer that arrives after a newer query is dropped.

### Decision rules

In order, for the word under the cursor:

1. The likeliest word the history supports, from `TokenModel::predict`. A word
   the completion also offers weighs 1.5 times more and is always shown. One it
   does not offer must reach a probability of 0.5 (0.4 for a whole next word),
   and must not name a missing file.
2. Otherwise, a word the completion offers. With one candidate, that one. With
   several, the language model chooses, and the shortest stands in when the
   model does not answer. Two restrictions apply when nothing of the word has
   been typed: file names are not candidates, and the model's choice among
   several must reach a probability of 0.25, with no fallback.
3. Otherwise, for a command name that matches nothing as typed, the closest
   known command within one edit.
4. Otherwise, the model's own continuation of the line, when its first word
   reaches a probability of 0.5 and does not name a missing file.

When what has been typed is already a whole valid word, the suggestion is the
next word, from rule 1 applied to the next position.

The model is never asked about a line that looks like it carries a credential
(`looks_secret` in `llm.rs`). Its answer to rule 4 goes through `Engine::settle`,
which applies the missing-file check before anything is shown.

### Which words name files

`files.rs` decides whether a guessed word names a path that can be checked: it
contains `/`, starts with `~` or `.`, or ends in an extension of one to five
characters with a letter in it, which keeps `v1.2.3` out. Words that take the
shell to resolve, such as `$HOME/x`, `*.rs`, `host:path` or `key=value`, are
left alone. The engine stats at most 32 paths per query.

The check matters because the token model remembers `status.txt` as it
remembers `status`. Without it, a file read once in one directory is suggested
in every other.

### The history model

`tokens.rs` splits past commands into simple commands and those into words, and
counts each word in the contexts it appeared in, from the exact words before it
down to any argument of the same command. Prediction mixes the distributions of
the contexts that have data. Counts decay with a half-life, and commands that
failed weigh less. Only the first 64 words of a simple command are learned. The
module documentation lists the contexts and their weights.

## Protocol

One line per message, tab-separated fields. Text fields escape backslash, tab,
newline and carriage return. List items are separated by `0x1f`.

```
shell  → engine   Q <id> <cwd> <line> <word_start> <match> <candidates>
shell  → engine   N <names>
engine → shell    R <id> <edit> <text> <source>
```

`word_start` is the character index where the completion sees the current word
start. `match` is `prefix`, `fuzzy` or empty. Each candidate starts with its
kind, `f` or `w`. `edit` is `+` to append `text`, `=` to replace the current
word, empty for no suggestion, and `!` when the query could not be understood,
with the reason in `text`.

The protocol has a version, `VERSION` in `src/protocol.rs` and
`_ble_augur_protocol` in `shell/augur.bash`. The shell asks `augur protocol`
before starting the engine and passes `--protocol` to `augur serve`, which
refuses any other. The version exists because a shell keeps the functions it
sourced for days while `cargo install` replaces the engine under it. Before the
version, that mismatch produced an engine that answered every query with
nothing and a shell with no suggestions and no error.

## Files

| path                                        | written by          | content                           |
|---------------------------------------------|---------------------|-----------------------------------|
| `$XDG_STATE_HOME/augur/history.tsv`         | the shell           | one record per executed command   |
| `$XDG_STATE_HOME/augur/imported.tsv`        | `augur import`      | snapshot of `~/.bash_history`     |
| `<ble.sh runtime dir>/<pid>.augur.log`      | the engine (stderr) | diagnostics of the running engine |

`AUGUR_STATE_DIR` overrides the state directory, and both state files are
created with mode 0600 in a directory with mode 0700. ble.sh's runtime directory
is usually `$XDG_RUNTIME_DIR/blesh`; `ble-augur status` prints the log's path.
