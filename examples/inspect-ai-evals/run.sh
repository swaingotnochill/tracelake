#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

if [ ! -d .venv ]; then
  python3 -m venv .venv
  .venv/bin/pip install -q inspect-ai openai
fi

.venv/bin/inspect eval task.py \
  --model openai-api/zai/glm-4.7 \
  --model-base-url https://api.z.ai/api/paas/v4 \
  --limit 6 \
  --log-dir logs

.venv/bin/inspect log dump "$(ls -t logs/*.eval | head -1)" > evals.json

cd ../..
cargo run -q -p tracelake-server --example inspect_ai_demo
