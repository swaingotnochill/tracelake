# inspect-ai evals in TraceLake

This folder runs a real inspect-ai eval, exports the log, and loads it into
TraceLake. The eval here is small on purpose: six samples, two scorers, one
model. The point is the path from `inspect eval` to a typed table you can query.

## Run it

```bash
./run.sh
```

`run.sh` creates a Python venv, installs `inspect-ai`, runs the eval against
Z.ai's GLM model (`ZAI_API_KEY` must be set), dumps the log to `evals.json`, and
then runs the Rust example that ingests and queries it.

The query output looks like this:

```
all samples across two segments
sample_id    task              score   tokens
arith-1@1    tracelake_demo     1.00       77
...
6 rows, segments scanned 2, pruned 0

score >= 0.5
...
3 rows, segments scanned 1, pruned 1
```

The second query prunes one of the two segments because its per-field stats say
every score in it is below 0.5.

## Run the eval yourself

```bash
python3 -m venv .venv
.venv/bin/pip install inspect-ai openai

.venv/bin/inspect eval task.py \
  --model openai-api/zai/glm-4.7 \
  --model-base-url https://api.z.ai/api/paas/v4 \
  --limit 6 --log-dir logs

.venv/bin/inspect log dump "$(ls -t logs/*.eval | head -1)" > evals.json

cd ../..
cargo run -p tracelake-server --example inspect_ai_demo
```

Any inspect model works. `inspect eval task.py --model mockllm/model` runs the
same task offline with a deterministic mock model, which is useful for a
repeatable log without an API key.

## What the database stores

`POST /v1/evals` and `ingest_eval_log` flatten the log into rows of
`inspect_ai_samples`. The score column uses the first scorer by name unless you
pass one to `sample_records`. The full scorer map, including answers and
explanations, is kept in the `scores` JSON column.

Pass the log to the HTTP API instead of the Rust example:

```bash
curl -s localhost:8080/v1/evals \
  -H 'content-type: application/json' \
  -d "$(jq -n --arg t demo --arg n evals \
        --slurpfile log evals.json \
        '{tenant_id:$t, namespace:$n, log:$log[0]}')" | jq
```

```bash
curl -s localhost:8080/v1/tables/demo/evals/inspect_ai_samples/query \
  -H 'content-type: application/json' \
  -d '{"filter":{"ge":{"field":"score","value":0.5}},
       "projection":["sample_id","task","score"]}' | jq
```
