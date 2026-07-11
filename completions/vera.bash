_vera_complete() {
  local current="${COMP_WORDS[COMP_CWORD]}"
  local commands="auth models session inspect mcp plugin update"
  if [[ "$COMP_CWORD" -eq 1 ]]; then
    COMPREPLY=( $(compgen -W "$commands" -- "$current") )
  fi
}
complete -F _vera_complete vera

