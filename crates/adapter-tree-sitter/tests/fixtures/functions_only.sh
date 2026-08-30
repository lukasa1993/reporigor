#!/usr/bin/env bash

# A shebang and comments are metadata, not executable top-level script bodies.
choose() {
  if [[ "$1" -gt 0 ]]; then
    printf '%s\n' "$1"
  else
    printf '%s\n' "none"
  fi
}
