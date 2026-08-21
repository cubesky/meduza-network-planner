#!/bin/sh
set -eu

cd "$(dirname "$0")"
exec "${PYTHON:-python3}" -m unittest discover -v -s . -p 'test_*.py'
