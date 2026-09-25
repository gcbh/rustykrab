#!/usr/bin/env bash
# Full verification matrix: every scenario on each build; Telegram cases repeated.
set -uo pipefail
V="$(cd "$(dirname "$0")" && pwd)"; cd "$V"
rm -rf matrix && mkdir matrix
for b in base pr661 pr662; do
  for s in "http-batch-tools:verify: batch tools_list" "http-batch-cred:verify: batch credential" \
           "http-batch-todo:verify: batch todo" "http-silence:verify: answer then silence" \
           "http-silent-first:verify: silent first"; do
    python3 run_scenario.py bin/rustykrab-cli-$b $b "${s#*:}" matrix/$b-${s%%:*} >/dev/null 2>&1
  done
  for rep in 1 2 3; do
    python3 run_telegram.py bin/rustykrab-cli-$b $b "verify: batch credential" matrix/$b-tg-batch-cred-r$rep >/dev/null 2>&1
    python3 run_telegram.py bin/rustykrab-cli-$b $b "verify: answer then silence" matrix/$b-tg-silence-r$rep >/dev/null 2>&1
  done
  for rep in 1 2; do
    python3 run_telegram.py bin/rustykrab-cli-$b $b "verify: schedule credential job" matrix/$b-tg-cron-cred-r$rep "batch credential done" >/dev/null 2>&1
  done
done
echo MATRIX-DONE
