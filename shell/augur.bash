# augur: inline command suggestions for ble.sh, one word at a time.
#
# Source this file from the ble.sh init file (~/.blerc or
# ~/.config/blesh/init.sh):
#
#   source /path/to/augur/shell/augur.bash
#
# It does two independent things:
#
#   - Records every executed command with its directory, session, exit status
#     and duration to history.tsv in $AUGUR_STATE_DIR (default:
#     ${XDG_STATE_HOME:-~/.local/state}/augur), created with mode 0600. Commands
#     excluded by HISTCONTROL=ignorespace/ignoreboth, HISTIGNORE or
#     `bleopt augur_ignore` are not recorded.
#   - Makes "augur" the first ble.sh auto-complete source. While you type, it
#     collects what ble.sh's completion offers for the word under the cursor
#     (commands, subcommands, files, and whatever the completion scripts list,
#     such as pods or branches) and asks `augur serve`, a background process per
#     shell, which word comes next. The engine is optional; without it, ble.sh
#     behaves as usual.
#
# Options, set with `bleopt name=value` after sourcing this file:
#
#   augur_command   the augur executable                            [augur]
#   augur_timeout   milliseconds to wait for a suggestion           [1000]
#   augur_model     Ollama model for the engine to ask when the      ['']
#                   history cannot decide; 'none' to never ask
#   augur_fallback  ble.sh sources to try when augur has nothing to  ['']
#                   suggest, e.g. 'history syntax'
#   augur_ignore    colon-separated patterns never recorded         ['']
#   augur_log       file receiving the engine's diagnostics; by     ['']
#                   default one per shell in ble.sh's runtime
#                   directory, which `ble-augur status` names
#
# `ble-augur status` shows the engine state and the last problem, if any.
# `ble-augur restart` sources this file again and restarts the engine: run it
# after installing a new build.
#
# Names starting with _ble_ or ble/ that are not defined here belong to ble.sh.
# shellcheck shell=bash disable=SC2154

if [[ ! ${BLE_VERSION-} ]]; then
  printf '%s\n' 'augur: ble.sh is not loaded; source augur.bash from the ble.sh init file' >&2
  return 1
fi

ble-import util.bgproc

# The protocol spoken here; VERSION in src/protocol.rs.
_ble_augur_protocol=2
# Absolute, for `ble-augur restart` to source this file from any directory and
# never a file of the same relative path in a directory someone else wrote.
_ble_augur_script=${BASH_SOURCE[0]}
[[ $_ble_augur_script == /* ]] || _ble_augur_script=$PWD/$_ble_augur_script

# When this file is sourced again, the engine running was started by the
# functions sourced before, which may speak another protocol.
ble/util/bgproc#opened _ble_augur && ble/util/bgproc#close _ble_augur

bleopt/declare -v augur_command augur
bleopt/declare -n augur_timeout 1000
bleopt/declare -v augur_model ''
bleopt/declare -v augur_fallback ''
bleopt/declare -v augur_ignore ''
bleopt/declare -v augur_log ''

# Same rules as StateDir::locate in src/state.rs: absolute paths only.
if [[ ${AUGUR_STATE_DIR-} == /* ]]; then
  _ble_augur_state_dir=$AUGUR_STATE_DIR
elif [[ ${XDG_STATE_HOME-} == /* ]]; then
  _ble_augur_state_dir=$XDG_STATE_HOME/augur
else
  _ble_augur_state_dir=$HOME/.local/state/augur
fi
_ble_augur_history_file=$_ble_augur_state_dir/history.tsv

# Sets ret to $1 with backslash, tab, newline and carriage return escaped, as
# src/escape.rs expects. The replacements are quoted so that patsub_replacement
# (bash 5.2+) cannot reinterpret backslashes or '&'.
function ble/augur/.escape {
  local text=$1 bs=\\
  text=${text//"$bs"/"$bs$bs"}
  text=${text//$'\t'/"${bs}t"}
  text=${text//$'\n'/"${bs}n"}
  ret=${text//$'\r'/"${bs}r"}
}

#------------------------------------------------------------------------------
# Recording

_ble_augur_exec_id=
_ble_augur_exec_command=
_ble_augur_exec_cwd=
_ble_augur_exec_start=
_ble_augur_state_ready=

function ble/augur/.is-ignored {
  local command=$1
  if [[ $command == [[:blank:]]* ]] &&
    [[ :${HISTCONTROL-}: == *:ignorespace:* || :${HISTCONTROL-}: == *:ignoreboth:* ]]; then
    return 0
  fi
  local patterns pattern
  ble/string#split patterns : "${HISTIGNORE-}:$bleopt_augur_ignore"
  for pattern in "${patterns[@]}"; do
    [[ $pattern && $pattern != '&' ]] || continue
    # shellcheck disable=SC2053 # the right-hand side is a pattern on purpose
    [[ $command == $pattern ]] && return 0
  done
  return 1
}

function ble/augur/.prepare-state-dir {
  [[ $_ble_augur_state_ready ]] && return 0
  if [[ ! -d $_ble_augur_state_dir ]]; then
    (umask 077 && mkdir -p -- "$_ble_augur_state_dir") 2>/dev/null || return 1
  fi
  if [[ ! -e $_ble_augur_history_file ]]; then
    (umask 077 && : >>"$_ble_augur_history_file") 2>/dev/null || return 1
  fi
  # Once per shell: a directory restored from a backup, or made by hand, may be
  # open to others.
  chmod 700 -- "$_ble_augur_state_dir" 2>/dev/null
  chmod 600 -- "$_ble_augur_history_file" 2>/dev/null
  _ble_augur_state_ready=1
}

# PREEXEC runs right before each command line executes, so the directory and
# start time are those of the command even when several lines were queued.
function ble/augur/preexec.hook {
  _ble_augur_exec_id=
  # `set +o history` is how one keeps a command out of the history.
  [[ -o history ]] || return 0
  ble/augur/.is-ignored "$1" && return 0
  _ble_augur_exec_id=$_ble_edit_exec_command_id
  _ble_augur_exec_command=$1
  _ble_augur_exec_cwd=$PWD
  _ble_augur_exec_start=$EPOCHSECONDS
}

function ble/augur/postexec.hook {
  [[ $_ble_augur_exec_id && $_ble_augur_exec_id == "$_ble_edit_exec_command_id" ]] || return 0
  _ble_augur_exec_id=
  ble/augur/.prepare-state-dir || return 0

  local ret session cwd command duration=
  ble/augur/.escape "${BLE_SESSION_ID-}"; session=$ret
  ble/augur/.escape "$_ble_augur_exec_cwd"; cwd=$ret
  ble/augur/.escape "$_ble_augur_exec_command"; command=$ret
  [[ $_ble_exec_time_ata ]] && duration=$((_ble_exec_time_ata / 1000))
  printf '1\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$_ble_augur_exec_start" "$duration" "$_ble_edit_exec_lastexit" \
    "$session" "$cwd" "$command" 2>/dev/null >>"$_ble_augur_history_file"
}

blehook PREEXEC!=ble/augur/preexec.hook
blehook POSTEXEC!=ble/augur/postexec.hook

#------------------------------------------------------------------------------
# Suggestions

_ble_augur_query_id=0
_ble_augur_disabled=
_ble_augur_error=
_ble_augur_starts=0 # engine starts since its last answer

# Sets ret to the file receiving the engine's diagnostics.
function ble/augur/.log-file {
  ret=${bleopt_augur_log:-$_ble_base_run/$$.augur.log}
}

function ble/augur/.engine-proc {
  local -a args=(serve --protocol "$_ble_augur_protocol" --owner-pid "$$" --session "${BLE_SESSION_ID-}")
  case $bleopt_augur_model in
  ('') ;;
  (none) args+=(--no-model) ;;
  (*) args+=(--model "$bleopt_augur_model") ;;
  esac
  local ret
  ble/augur/.log-file
  exec "$bleopt_augur_command" "${args[@]}" 2>>"$ret"
}

# Tells the engine the functions, aliases and builtins this shell defines, so
# that it can correct mistyped command names. Names holding a / (ble.sh's own
# functions) or starting with _ (completion helpers) are left out: nobody types
# them.
function ble/augur/.send-names {
  local -a names=()
  ble/util/assign-array names 'compgen -A function -A alias -A builtin -A keyword'
  local list='' name ret
  for name in "${names[@]}"; do
    [[ $name == */* || $name == _* || $name == *[[:cntrl:]]* ]] && continue
    ble/augur/.escape "$name"
    list+=${list:+$'\x1f'}$ret
  done
  # Not ble/util/bgproc#post: it restarts an engine that exited, which runs
  # this function again, without end when the engine exits as it starts.
  ble/util/print "N"$'\t'"$list" 2>/dev/null 1>&"${_ble_augur_bgproc[1]}"
}

function ble/util/bgproc/onstart:_ble_augur {
  ble/augur/.send-names
}

# Starts the engine, unless it has exited three times without an answer in
# between: an engine that cannot run, e.g. over its state directory, would
# otherwise be started at every keystroke. The last line it wrote says why.
function ble/augur/.start {
  local ret
  ble/augur/.log-file
  if ((_ble_augur_starts >= 3)); then
    local line reason=
    while IFS= read -r line || [[ $line ]]; do
      [[ $line ]] && reason=$line
    done 2>/dev/null <"$ret"
    _ble_augur_disabled="the engine keeps exiting${reason:+: ${reason#augur: }}"
    ble/util/bgproc#opened _ble_augur && ble/util/bgproc#close _ble_augur
    return 1
  fi
  ((_ble_augur_starts++))
  # The default log holds the diagnostics of the running engine only.
  [[ $bleopt_augur_log ]] || : 2>/dev/null >|"$ret"
  if ble/util/bgproc#opened _ble_augur; then
    ble/util/bgproc#start _ble_augur
  else
    ble/util/bgproc#open _ble_augur ble/augur/.engine-proc kill-timeout=0
  fi
}

# Starts the engine on first use and again when it exited.
function ble/augur/.connect {
  [[ $_ble_augur_disabled ]] && return 1
  if ble/util/bgproc#opened _ble_augur; then
    ble/util/bgproc#alive _ble_augur && return 0
    ble/augur/.start
    return "$?"
  fi
  if ! type -P -- "$bleopt_augur_command" &>/dev/null; then
    _ble_augur_disabled="'$bleopt_augur_command' not found"
    return 1
  fi
  local protocol
  # shellcheck disable=SC2016 # ble/util/assign evaluates the command
  ble/util/assign protocol '"$bleopt_augur_command" protocol 2>/dev/null'
  if [[ $protocol != "$_ble_augur_protocol" ]]; then
    _ble_augur_disabled="'$bleopt_augur_command' speaks protocol ${protocol:-1} and this shell protocol $_ble_augur_protocol; install the matching build, then run 'ble-augur restart'"
    return 1
  fi
  ble/augur/.start
}

function ble/augur/.fallback {
  local source ext
  local -a sources
  ble/string#split-words sources "$bleopt_augur_fallback"
  for source in "${sources[@]}"; do
    ble/is-function ble/complete/auto-complete/source:"$source" || continue
    ble/complete/auto-complete/source:"$source"; ext=$?
    ((ext == 0 || ext == 148)) && return "$ext"
  done
  # Stop ble.sh from trying its other sources.
  return 0
}

# fzf's completion integration takes over the completion of some sixty
# commands (git, tar, grep, cp…) to offer its '**' path completion. While
# auto-completing, ble.sh makes it return nothing, and the command's own
# completion, which fzf loads on the first TAB, is never consulted. During
# auto-completion only, go straight to fzf's loader: it runs the command's
# completion, loading it first if needed. TAB keeps fzf's behaviour.
function ble/augur/fzf-path-completion.advice {
  if [[ :${comp_type-}: != *:auto:* ]] || ! ble/is-function _fzf_handle_dynamic_completion; then
    ble/function#advice/do
    return
  fi
  local cmd=${ADVICE_WORDS[1]}
  cmd=${cmd#\\}
  _fzf_handle_dynamic_completion "$cmd" "${ADVICE_WORDS[@]:1}" &>/dev/null
  ADVICE_EXIT=$?
  if ((ADVICE_EXIT == 124)); then
    # The completion was loaded just now; run it.
    _fzf_handle_dynamic_completion "$cmd" "${ADVICE_WORDS[@]:1}" &>/dev/null
    ADVICE_EXIT=$?
  fi
}

function ble/augur/.adapt-fzf {
  ble/is-function _fzf_path_completion || return 0
  ble/is-function ble/function#advice/around:_fzf_path_completion && return 0
  ble/function#advice around _fzf_path_completion ble/augur/fzf-path-completion.advice
}

# Sets candidates, matching and COMP1 from what ble.sh's completion offers for
# the word under the cursor. Returns 148 when the user typed meanwhile.
function ble/augur/.complete {
  # At every query: fzf's integration may be loaded after this file.
  ble/augur/.adapt-fzf
  # These locals are read and set by ble.sh's completion functions, which see
  # them through bash's dynamic scoping.
  # shellcheck disable=SC2034
  local sources
  ble/complete/context:syntax/generate-sources "$comp_text" "$comp_index" || return 0
  # shellcheck disable=SC2034
  local bleopt_complete_contract_function_names=''
  local bleopt_complete_menu_style=$bleopt_complete_menu_style
  # As ble.sh's own auto-complete source does: check for a keystroke at least
  # every 25 candidates.
  ((bleopt_complete_polling_cycle > 25)) && local bleopt_complete_polling_cycle=25
  ble/complete/candidates/generate; local ext=$?
  ((ext == 148)) && return 148
  ((cand_count)) || return 0
  matching=prefix
  [[ :$comp_type: == *:[maA]:* ]] && matching=fuzzy
  # Each candidate goes with its kind: f for a file name, w for any other word.
  # ble.sh lists the directory for every command it has no completion for, so
  # the engine does not take file names for the words the command expects.
  # Abbreviations (ble-sabbrev) are shorthands to expand, not words to predict.
  # The engine chooses among 200 words at most, and a query must fit the pipe to
  # it: a write that does not would block the shell if the engine stopped
  # reading.
  local i word kind ret n=0 size=0
  # shellcheck disable=SC2034 # set by ble/complete/cand/unpack
  local ACTION CAND INSERT DATA PREFIX_LEN
  for ((i = 0; i < cand_count; i++)); do
    kind=w
    case ${cand_pack[i]%%:*} in
    (sabbrev) continue ;;
    (file) kind=f ;;
    (progcomp)
      ble/complete/cand/unpack "${cand_pack[i]}"
      [[ $DATA == *:filenames:* ]] && kind=f ;;
    esac
    word=${cand_word[i]}
    [[ $word == *[[:cntrl:]]* ]] && continue
    ((${#word} <= 256)) || continue
    ((size += ${#word} + 2, size <= 16384)) || break
    # Without control characters, only a backslash is left to escape.
    ret=$word
    [[ $word == *\\* ]] && ble/augur/.escape "$word"
    candidates+=${candidates:+$'\x1f'}$kind$ret
    ((++n >= 400)) && break
  done
  return 0
}

# Remembers why the engine gave no answer, for `ble-augur status`, and returns
# 1: ble.sh falls back to its own sources.
function ble/augur/.fail {
  _ble_augur_error=$1
  return 1
}

# Returns 0 after showing a suggestion or deciding to show none, 148 when the
# user typed meanwhile, and 1 when augur cannot answer, letting ble.sh fall back
# to its own sources.
function ble/complete/auto-complete/source:augur {
  [[ $_ble_history_prefix ]] && return 1
  ((_ble_edit_ind == ${#_ble_edit_str})) || return 1
  [[ $_ble_edit_str == *$'\n'* ]] && return 1
  ble/augur/.connect || return 1

  # Set by ble.sh's completion in ble/augur/.complete, through dynamic scoping.
  # shellcheck disable=SC2034
  local comp_type=$comp_type COMP1='' COMP2='' COMPS='' COMPV='' comps_flags='' comps_fixed=''
  # shellcheck disable=SC2034
  local cand_count=0 cand_cand=() cand_word=() cand_pack=() cand_limit_reached=''
  local candidates='' matching=
  ble/augur/.complete || return "$?"

  local fd_response=${_ble_augur_bgproc[0]} fd_request=${_ble_augur_bgproc[1]}
  local id=$((++_ble_augur_query_id)) ret cwd line
  ble/augur/.escape "$PWD"; cwd=$ret
  ble/augur/.escape "$_ble_edit_str"; line=$ret
  if ! ble/util/print "Q"$'\t'"$id"$'\t'"$cwd"$'\t'"$line"$'\t'"$COMP1"$'\t'"$matching"$'\t'"$candidates" 2>/dev/null 1>&"$fd_request"; then
    ble/augur/.fail 'cannot write to the engine'
    return 1
  fi

  # Poll in short slices so that a keystroke cancels the wait within ~4ms.
  # EPOCHREALTIME's decimal separator follows the locale; without it, the value
  # is in microseconds.
  local now=${EPOCHREALTIME//[!0-9]}
  local deadline=$((now + bleopt_augur_timeout * 1000))
  local reply='' chunk ext
  while :; do
    chunk=
    IFS= ble/bash/read-timeout 0.004 -r chunk <&"$fd_response"; ext=$?
    reply+=$chunk
    if ((ext == 0)); then
      [[ $reply == R$'\t'"$id"$'\t'* ]] && break
      reply= # the answer to an abandoned query
      continue
    fi
    if ((ext <= 128)); then
      ble/augur/.fail 'cannot read from the engine'
      return 1
    fi
    ble/decode/has-input && return 148
    now=${EPOCHREALTIME//[!0-9]}
    if ((now >= deadline)); then
      if ble/util/bgproc#alive _ble_augur; then
        ble/augur/.fail "no answer within ${bleopt_augur_timeout}ms"
      else
        ble/augur/.fail 'the engine exited'
      fi
      return 1
    fi
  done

  _ble_augur_error=
  _ble_augur_starts=0
  local rest=${reply#R$'\t'"$id"$'\t'}
  local edit=${rest%%$'\t'*}
  rest=${rest#*$'\t'}
  local text=${rest%%$'\t'*}
  case $edit in
  (+) ble/complete/auto-complete/enter h 0 "$text" '' "$_ble_edit_str$text" ;;
  (=) local start=$COMP1
      if [[ ! $start ]]; then
        local word=${_ble_edit_str##*[$' \t|&;()<>']}
        start=$((${#_ble_edit_str} - ${#word}))
      fi
      ble/complete/auto-complete/enter r "$start" " [$text] " "$text" "$text" "$text" '' ;;
  (!) ble/augur/.fail "the engine rejected the query: $text"
      return 1 ;;
  (*) ble/augur/.fallback ;;
  esac
}

function ble/augur/.register-source {
  ble/array#remove _ble_complete_auto_source augur
  ble/array#unshift _ble_complete_auto_source augur
}
ble/util/import/eval-after-load core-complete ble/augur/.register-source

# Start the engine once the prompt is idle, so the first suggestion does not
# wait for it to load the history.
ble/is-function ble/util/idle.push && ble/util/idle.push ble/augur/.connect

function ble-augur {
  case ${1-status} in
  (status)
    local state=stopped
    if [[ $_ble_augur_disabled ]]; then
      state="disabled: $_ble_augur_disabled"
    elif ble/util/bgproc#alive _ble_augur; then
      state="running, pid ${_ble_augur_bgproc[4]}"
    fi
    ble/util/print "engine:   $state"
    ble/util/print "command:  $bleopt_augur_command"
    ble/util/print "protocol: $_ble_augur_protocol"
    ble/util/print "model:    ${bleopt_augur_model:-default}"
    ble/util/print "history:  $_ble_augur_history_file"
    local ret
    ble/augur/.log-file
    ble/util/print "log:      $ret"
    [[ $_ble_augur_error ]] && ble/util/print "problem:  $_ble_augur_error"
    return 0
    ;;
  (restart)
    # The file may have changed along with the engine. Sourcing it stops the
    # engine, resets the state above and keeps the options: bleopt/declare
    # leaves set ones alone.
    # shellcheck disable=SC1090
    if ! source "$_ble_augur_script"; then
      ble/util/print "augur: cannot source $_ble_augur_script" >&2
      return 1
    fi
    if ble/augur/.connect; then
      ble/util/print 'augur: engine restarted'
    else
      ble/util/print "augur: ${_ble_augur_disabled:-the engine did not start}" >&2
      return 1
    fi
    ;;
  (*)
    ble/util/print 'usage: ble-augur [status|restart]' >&2
    return 2
    ;;
  esac
}
