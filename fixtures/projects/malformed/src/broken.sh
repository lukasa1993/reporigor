#!/usr/bin/env bash

broken() {
  if [[ "$1" == yes ]]; then
    printf '%s\n' yes
