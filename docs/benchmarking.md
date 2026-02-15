# Benchmarking Canopy

This document defines the current local benchmarking methodology for canopy.

No canonical performance claims are published yet. Use this protocol to run reproducible comparisons and inspect retrieval behavior.

## Scripts

- Multi-agent swarm: `benchmark/run-swarm-test.sh`
- Single-agent A/B: `benchmark/run-ab-test.sh`

## Recommended Compare Run

```bash
MODE=compare COMPARE_MODES="baseline canopy canopy-service" \
AGENTS=4 MAX_TURNS=5 INDEX_TIMEOUT=1200 \
./benchmark/run-swarm-test.sh /path/to/repo
```

Output directory:
- `benchmark/results/swarm-<timestamp>/`

Key artifacts:
- `summary.md`: aggregate and per-agent rollup
- `metrics.jsonl`: per-agent raw metrics
- `<mode>/agent-*.json`: raw model output payloads
- `<mode>/agent-*.md`: readable task outputs
- `canopy-service/service-metrics.json`: service endpoint counters
- `<mode>/local-feedback-metrics.json`: local feedback event snapshot (if available)

## Modes

- `baseline`: no canopy MCP
- `canopy`: local canopy MCP (standalone mode)
- `canopy-service`: canopy MCP backed by `canopy-service`

## Metric Definitions

### Token Metrics

- Reported tokens:
  - `input_tokens + cache_creation_input_tokens + output_tokens`
  - aligns with the benchmark script's `total_tokens`
- Effective tokens:
  - `reported_tokens + cache_read_input_tokens`
  - useful for total context-consumption analysis
- Cache-read tokens:
  - tracked separately; high values can hide in reported-only views

### Time and Cost

- Agent duration: wall-clock duration of each agent run
- Max agent time: worst-case latency within a mode
- Total cost: sum of per-agent `total_cost_usd`

### Stability / Tail Behavior

- Compactions:
  - currently proxied as `num_turns >= MAX_TURNS`
  - indicates turn-budget saturation and potential answer truncation risk

### Output Quality Heuristics

Heuristic section in summary includes:
- Success rate: non-empty and not compacted
- Average output lines
- Average turns used
- Null/empty result count

Strict heuristic section includes:
- Grounded outputs: responses with at least 3 file-path references
- Average file references/output
- Structured outputs: at least 3 headings and at least 5 bullets or 1 code fence

### Retrieval Path Verification

Use both local and service counters:
- Local query events / expand events from `.canopy/feedback.db`
- Service `/query` and `/expand` call counts from `service-metrics.json`

If `canopy-service` shows `queries=0` and `expands=0`, the run did not exercise service retrieval even if service reindex happened.

## Reproducibility Protocol

1. Keep setup controlled.
   - same repository, same model, same tasks, same `AGENTS`, same `MAX_TURNS`
2. Compare all target modes in a single run when possible.
3. Inspect both reported and effective token views.
4. Check compactions and max-agent-time for worst-case behavior.
5. Validate retrieval-path counters before interpreting canopy-service results.
6. Prefer multiple runs and summarize medians, not single-run outliers.

## Troubleshooting

### `awk` or summary-generation errors

- Ensure shell compatibility (macOS Bash 3.x quirks are common).
- Re-run with latest `benchmark/run-swarm-test.sh`.

### canopy-service appears unused

Symptoms:
- `service-metrics.json` has `queries=0`, `expands=0`, `reindexes>0`

Checks:
1. Confirm mode is `canopy-service`.
2. Confirm MCP config passed to agents includes service URL.
3. Check agent stderr logs in `<mode>/agent-*.stderr.log`.
4. Verify service is healthy (`/status`) during agent execution.

### Empty or missing agent JSON

- Script writes fallback JSON when agent command fails.
- Inspect `<mode>/agent-*.stderr.log` and command exit status messages.
