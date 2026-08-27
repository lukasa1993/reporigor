#!/usr/bin/env bash

greet() {
  if [[ "$1" == "world" && "$2" -gt 0 ]]; then
    echo "hello"
  else
    false
  fi
}

echo "ready"
