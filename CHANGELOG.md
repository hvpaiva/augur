# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
augur has no releases yet. Until it does, the protocol version is what tells
two builds apart: the shell integration and the engine must speak the same one.

## Unreleased

First public version. Protocol 2.

- Word-by-word suggestions for ble.sh's auto-complete, from the shell's
  completion, the command history and an optional local language model. Paths
  guessed from the history come one component at a time, and the arguments of
  other commands are consulted when the command's own history has nothing.
- A history log with the directory, session, exit status and duration of every
  command, and `augur import` to start it from `~/.bash_history`.
- Correction of mistyped command names.
- `augur eval`, which replays the history and compares augur with ble.sh's
  history source, and `augur suggest`, which explains one suggestion.
- A versioned protocol between the shell integration and the engine, checked
  before the engine starts.
