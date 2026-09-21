# Security

## Reporting a vulnerability

Use GitHub's private reporting: the "Report a vulnerability" button under the
Security tab of <https://github.com/hvpaiva/augur>. Please do not open a public
issue for something exploitable. Expect a first answer within a week. augur has
no releases yet, so fixes land on `main`.

## What augur touches

augur runs inside your interactive shell and sees every command line you type
and run. Three things follow from that.

It writes a log. `~/.local/state/augur/history.tsv` has one record per executed
command with its directory, session, exit status and duration. The file is
created with mode 0600 in a directory with mode 0700, and both modes are set
again once per shell in case the directory came from elsewhere. It is as
sensitive as `~/.bash_history` and a little more, because of the directories.
Commands that bash would keep out of its history (`set +o history`,
`HISTCONTROL=ignorespace`, `HISTIGNORE`) are kept out of this one too, and
`bleopt augur_ignore` adds patterns of your own. `augur import` is the
exception: it copies `~/.bash_history` as it is.

It puts text on your command line. A suggestion is text from your history, from
the shell's completion (which includes file and branch names, so content an
attacker can choose by getting you to clone a repository), or from the language
model. augur never runs anything: a suggestion is shown, and inserted only when
you accept it. Suggestions are limited to 120 bytes of one line and are dropped
when they contain control characters, blanks other than the space,
bidirectional overrides, zero-width characters, fillers, variation selectors or
tag characters, so that what you accept is what you saw. The list is explicit
(`is_invisible` in `src/suggest.rs`), not the whole Unicode `Cf` category, and
it does nothing about homoglyphs. Read a suggestion before accepting it, as you
would a pasted command.

It talks to a language model, by default. That is Ollama on
`http://127.0.0.1:11434`, in plain HTTP and with no proxy, whatever
`http_proxy` says. The shell integration has no option to change the address.
`augur serve`, `suggest` and `eval` take `--ollama`, and an `https://` address
fails closed, since augur is built without TLS. A prompt holds the working
directory, the line being typed and up to twelve recent commands.

Lines containing `password`, `passwd`, `secret`, `token`, `apikey`, `api_key`,
`api-key`, `authorization`, `bearer`, `private_key`, `aws_access` or
`aws_secret` are left out of prompts, and when the line being typed contains
one, the model is not asked at all. That is a keyword list, not a secret
scanner. It misses `curl -u user:pass`, `https://user:pass@host`, `mysql -pXXX`,
`sshpass -p`, `docker login -p`, and tokens recognisable only by their shape,
such as `ghp_…`, `sk-…` or `AKIA…`.

Nothing authenticates the model. On a machine shared with other users, whoever
binds port 11434 while Ollama is not running receives those prompts and decides
what the model "suggests", within the limits above. `bleopt augur_model=none`
turns the model off, and is the right setting on such a machine.

## Out of scope

A local user who can already write to your state directory, your ble.sh runtime
directory or your `PATH` can do worse than influence suggestions.
