#!/usr/bin/env bash

primary_choice() {
  total=$(("$1" + "$2"))
  limit=10
  total=$((total * 2))
  if [[ "$1" -gt 0 && "$2" -ne 0 ]]; then
    total=$((total + limit))
  else
    total=$((total - limit))
  fi
  printf '%s\n' "$total"
}
