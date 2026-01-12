#!/usr/bin/env bash
set -euo pipefail

# Focused WPT subset for the js_html_integration workstream.
#
# This uses the repo-mandated hard time limit wrapper for all cargo commands.

timeout -k 10 600 bash scripts/cargo_agent.sh test -p js-wpt-dom-runner --features vmjs --test js_html_integration_wpt_subset
