# Completion harness for the GUI bash input (spec-gui "Command input").
# $1 = the command line up to the cursor. Prints the word being completed,
# then the candidates, all NUL-terminated.
#
# The line is NEVER eval'ed here: tokenization is plain whitespace
# splitting, so nothing the user typed can be expanded or executed by the
# harness itself. Programmable completion functions (bash-completion) may
# run their own helper commands, exactly like pressing Tab in a terminal.

line=$1

# No globbing while we shuffle user text through arrays.
set -f

# Tokenize on whitespace only; a trailing space starts a new, empty word.
words=()
read -ra words <<<"$line"
if ((${#words[@]} == 0)) || [[ -n $line && $line == *[[:space:]] ]]; then
  words+=('')
fi
cword=$((${#words[@]} - 1))
cmd=${words[0]}
cur=${words[cword]}
prev=$cmd
((cword > 0)) && prev=${words[cword - 1]}

# The replaced region: readline only replaces the tail after the last
# word-break character. Approximate its default breaks for the common
# "opening quote", --opt=value / VAR=value and host:path shapes.
case $cur in
  \"* | \'*) cur=${cur:1} ;;
esac
cur=${cur##*[=:]}

# This (possibly rewritten) prefix is matched; the on-screen tail is what
# the frontend replaces, so it is printed verbatim below.
display_cur=$cur
if [[ $cur == '~' || $cur == '~/'* ]]; then
  cur=$HOME${cur:1}
fi

candidates=()

# Programmable completion via the bash-completion package: resolve the
# command's spec function (loading it on demand) and call it with the
# COMP_* environment readline would provide.
complete_via_spec() {
  ((cword > 0)) || return 1
  [[ -r /usr/share/bash-completion/bash_completion ]] || return 1
  source /usr/share/bash-completion/bash_completion >/dev/null 2>&1 || return 1
  local spec
  spec=$(complete -p -- "$cmd" 2>/dev/null)
  if [[ -z $spec ]] && declare -F _completion_loader >/dev/null; then
    _completion_loader "$cmd" >/dev/null 2>&1
    spec=$(complete -p -- "$cmd" 2>/dev/null)
  fi
  [[ $spec =~ [[:space:]]-F[[:space:]]([^[:space:]]+) ]] || return 1
  local fn=${BASH_REMATCH[1]}
  COMP_LINE=$line
  COMP_POINT=${#line}
  COMP_WORDS=("${words[@]}")
  COMP_CWORD=$cword
  COMP_TYPE=9
  COMP_KEY=9
  COMPREPLY=()
  # compopt fails outside a real readline completion; that (and any other
  # hiccup inside the function) must not kill the harness.
  "$fn" "$cmd" "${words[cword]}" "$prev" >/dev/null 2>&1 || true
  ((${#COMPREPLY[@]} > 0)) || return 1
  candidates=("${COMPREPLY[@]}")
}

# Fallback: command names in command position, filenames elsewhere
# (directories decorated with a trailing slash, as readline displays them).
complete_fallback() {
  local entry entries=()
  if ((cword == 0)); then
    mapfile -t candidates < <(compgen -c -- "$cur" 2>/dev/null | LC_ALL=C sort -u)
  else
    mapfile -t entries < <(compgen -f -- "$cur" 2>/dev/null | LC_ALL=C sort)
    for entry in "${entries[@]}"; do
      [[ -d $entry ]] && entry+=/
      candidates+=("$entry")
    done
  fi
}

complete_via_spec || complete_fallback

printf '%s\0' "$display_cur"
if ((${#candidates[@]} > 0)); then
  printf '%s\0' "${candidates[@]}"
fi
exit 0
