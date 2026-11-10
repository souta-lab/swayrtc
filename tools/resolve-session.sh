#!/usr/bin/env bash
# Thin wrapper kept for the scripts that already call it.
exec "$(dirname "$(readlink -f "$0")")/resolve-session.py" "$@"
