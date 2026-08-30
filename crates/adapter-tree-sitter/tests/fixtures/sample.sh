#!/usr/bin/env bash

greet() {
  [[ "$1" == "world" ]] || return 1
  echo "hello"
}

echo "ready"
