#!/usr/bin/env bash

secondary_choice() {
  total=$(("$1" + "$2"))
  limit=25
  total=$((total * 3))
  if [[ "$1" -gt 1 && "$2" -ne 2 ]]; then
    total=$((total + limit))
  else
    total=$((total - limit))
  fi
  printf '%d\n' "$total"
}
