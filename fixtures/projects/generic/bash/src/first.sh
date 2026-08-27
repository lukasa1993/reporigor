#!/usr/bin/env bash

primary_choice() {
  if [[ "$1" == yes && "$2" != no ]]; then
    printf '%s\n' enabled
  else
    printf '%s\n' disabled
  fi
}
