#!/usr/bin/env bash
# NIM Gateway (Rust). Default port 8100.
set -e
cd "$(dirname "$0")"
mkdir -p data
exec ./target/release/nim-gateway --port 8100
