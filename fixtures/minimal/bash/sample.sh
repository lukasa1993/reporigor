#!/usr/bin/env bash
choose() {
  if [[ "$1" == yes && "$2" == yes ]]; then
    echo 1
  else
    echo 0
  fi
}
