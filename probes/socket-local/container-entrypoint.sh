#!/usr/bin/env bash
set -euo pipefail
# The timeout owns the launcher process tree inside this disposable container.
exec timeout --signal=TERM --kill-after=15s 15m /opt/socket-local/start.sh "$@"
