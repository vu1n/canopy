#!/usr/bin/env bash
set -e

# Swarm Benchmark: N concurrent agents, baseline vs canopy
# Measures: token economy, speed, quality across parallel agents

REPO="${1:?Usage: $0 /path/to/repo}"
AGENTS="${AGENTS:-5}"
MODE="${MODE:-}"  # "" = both, "baseline", or "canopy"
MODEL="${MODEL:-}"
MAX_TURNS="${MAX_TURNS:-15}"
DATE=$(date +%Y%m%d-%H%M%S)
OUTPUT_DIR="${OUTPUT_DIR:-benchmark/results/swarm-$DATE}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TASKS=(
  "Find all authentication-related middleware and explain the auth flow"
  "List all database models/schemas and their relationships"
  "Trace the request lifecycle from HTTP handler to response"
  "Find all background job processors and explain the job queue system"
  "Map the plugin/extension architecture — how are plugins loaded and executed?"
)

mkdir -p "$OUTPUT_DIR"

echo "=============================================="
echo "  Canopy Swarm Benchmark"
echo "=============================================="
echo "Repository:  $REPO"
echo "Agents:      $AGENTS"
echo "Max turns:   $MAX_TURNS"
echo "Mode:        ${MODE:-both}"
echo "Model:       ${MODEL:-default}"
echo "Output:      $OUTPUT_DIR"
echo "=============================================="
echo ""

# Save config
cat > "$OUTPUT_DIR/config.json" << EOF
{
  "repo": "$REPO",
  "agents": $AGENTS,
  "max_turns": $MAX_TURNS,
  "mode": "${MODE:-both}",
  "model": "${MODEL:-default}",
  "date": "$DATE",
  "tasks": $(printf '%s\n' "${TASKS[@]}" | jq -R . | jq -s .)
}
EOF

# Build canopy
echo "Building canopy..."
(cd "$PROJECT_ROOT" && cargo build --release -q 2>/dev/null)
echo ""

run_agent() {
  local mode=$1
  local agent_num=$2
  local task=$3
  local mode_dir="$OUTPUT_DIR/$mode"
  local json_file="$mode_dir/agent-${agent_num}.json"
  local md_file="$mode_dir/agent-${agent_num}.md"

  mkdir -p "$mode_dir"

  local cmd_args=(
    claude
    --dangerously-skip-permissions
    -p "$task"
    --output-format json
    --cwd "$REPO"
    --max-turns "$MAX_TURNS"
  )

  if [ -n "$MODEL" ]; then
    cmd_args+=(--model "$MODEL")
  fi

  if [ "$mode" = "canopy" ]; then
    local mcp_config
    mcp_config=$(cat << MCPEOF
{"mcpServers":{"canopy":{"command":"$PROJECT_ROOT/target/release/canopy-mcp","args":["--root","$REPO"]}}}
MCPEOF
    )
    cmd_args+=(--mcp-config "$mcp_config")
  fi

  # Run agent, capture output
  "${cmd_args[@]}" 2>/dev/null > "$json_file" || true

  # Extract metrics from JSON (default to 0 for missing/empty values)
  jq_num() { local v; v=$(jq -r "$1" "$2" 2>/dev/null); echo "${v:-0}"; }
  local duration_ms=$(jq_num '.duration_ms // 0' "$json_file")
  local num_turns=$(jq_num '.num_turns // 0' "$json_file")
  local input_tokens=$(jq_num '.usage.input_tokens // 0' "$json_file")
  local cache_read=$(jq_num '.usage.cache_read_input_tokens // 0' "$json_file")
  local cache_create=$(jq_num '.usage.cache_creation_input_tokens // 0' "$json_file")
  local output_tokens=$(jq_num '.usage.output_tokens // 0' "$json_file")
  local cost=$(jq_num '.total_cost_usd // 0' "$json_file")
  local result
  result=$(jq -r '.result // "No result"' "$json_file" 2>/dev/null) || result="No result"
  [ -z "$result" ] && result="No result"
  local result_lines=$(echo "$result" | wc -l | tr -d ' ')
  local duration_s=$(echo "scale=2; $duration_ms / 1000" | bc 2>/dev/null || echo "0")
  local total_tokens=$((input_tokens + cache_create + output_tokens))
  local compacted=0
  if [ "$num_turns" -ge "$MAX_TURNS" ] 2>/dev/null; then
    compacted=1
  fi

  # Save readable output
  cat > "$md_file" << HEREDOC
# [$mode] Agent $agent_num

## Task
$task

## Result
$result

---
## Metrics
- **Duration:** ${duration_s}s
- **Turns:** $num_turns
- **Input tokens:** $input_tokens (+ $cache_create cache creation)
- **Cache read:** $cache_read
- **Output tokens:** $output_tokens
- **Total tokens:** $total_tokens
- **Cost:** \$${cost}
- **Result lines:** $result_lines
- **Compacted:** $compacted
HEREDOC

  # Append to metrics.jsonl
  echo "{\"mode\":\"$mode\",\"agent\":$agent_num,\"task\":$(echo "$task" | jq -R .),\"duration_ms\":$duration_ms,\"duration_s\":$duration_s,\"num_turns\":$num_turns,\"input_tokens\":$input_tokens,\"cache_read\":$cache_read,\"cache_create\":$cache_create,\"output_tokens\":$output_tokens,\"total_tokens\":$total_tokens,\"cost\":$cost,\"result_lines\":$result_lines,\"compacted\":$compacted}" >> "$OUTPUT_DIR/metrics.jsonl"

  echo "  Agent $agent_num done: ${duration_s}s, ${total_tokens} tokens, \$$cost"
}

run_mode() {
  local mode=$1

  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo "  Running mode: $mode ($AGENTS agents)"
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

  # Clean slate for fair comparison
  rm -rf "$REPO/.canopy"

  local start_time=$(date +%s)
  local pids=()

  for i in $(seq 1 "$AGENTS"); do
    # Round-robin task assignment
    local task_idx=$(( (i - 1) % ${#TASKS[@]} ))
    local task="${TASKS[$task_idx]}"

    echo "  Starting agent $i: ${task:0:60}..."
    run_agent "$mode" "$i" "$task" &
    pids+=($!)
  done

  # Wait for all agents
  local failures=0
  for pid in "${pids[@]}"; do
    if ! wait "$pid"; then
      ((failures++)) || true
    fi
  done

  local end_time=$(date +%s)
  local wall_time=$((end_time - start_time))

  echo ""
  echo "  Mode $mode complete: ${wall_time}s wall time, $failures failures"
  echo ""
}

generate_summary() {
  local summary_file="$OUTPUT_DIR/summary.md"

  cat > "$summary_file" << 'HEADER'
# Swarm Benchmark Summary

HEADER

  # Add config info
  cat >> "$summary_file" << EOF
**Repository:** $REPO
**Agents:** $AGENTS | **Max turns:** $MAX_TURNS | **Model:** ${MODEL:-default}
**Date:** $DATE

---

## Per-Agent Results

| Mode | Agent | Task | Duration | Turns | Tokens | Cost | Lines |
|------|-------|------|----------|-------|--------|------|-------|
EOF

  if [ -f "$OUTPUT_DIR/metrics.jsonl" ]; then
    while IFS= read -r line; do
      local mode=$(echo "$line" | jq -r '.mode')
      local agent=$(echo "$line" | jq -r '.agent')
      local task=$(echo "$line" | jq -r '.task' | cut -c1-40)
      local duration=$(echo "$line" | jq -r '.duration_s')
      local turns=$(echo "$line" | jq -r '.num_turns')
      local tokens=$(echo "$line" | jq -r '.total_tokens')
      local cost=$(echo "$line" | jq -r '.cost' | xargs printf "%.4f" 2>/dev/null || echo "0")
      local lines=$(echo "$line" | jq -r '.result_lines')
      echo "| $mode | $agent | ${task}... | ${duration}s | $turns | $tokens | \$$cost | $lines |" >> "$summary_file"
    done < "$OUTPUT_DIR/metrics.jsonl"
  fi

  # Generate aggregate comparison if both modes ran
  local has_baseline=$(jq -r 'select(.mode=="baseline")' "$OUTPUT_DIR/metrics.jsonl" 2>/dev/null | head -1)
  local has_canopy=$(jq -r 'select(.mode=="canopy")' "$OUTPUT_DIR/metrics.jsonl" 2>/dev/null | head -1)

  if [ -n "$has_baseline" ] && [ -n "$has_canopy" ]; then
    cat >> "$summary_file" << 'DIVIDER'

---

## Aggregate Comparison

DIVIDER

    # Sum helper: reads lines of numbers, outputs their sum via awk
    sum_lines() { awk '{s+=$1} END {print s+0}'; }

    # Calculate aggregates per mode
    for mode in baseline canopy; do
      local total_tokens=$(jq -r "select(.mode==\"$mode\") | .total_tokens" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
      local total_cost=$(jq -r "select(.mode==\"$mode\") | .cost" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
      local max_duration=$(jq -r "select(.mode==\"$mode\") | .duration_s" "$OUTPUT_DIR/metrics.jsonl" | sort -n | tail -1)
      local agent_count=$(jq -r "select(.mode==\"$mode\") | .agent" "$OUTPUT_DIR/metrics.jsonl" | wc -l | tr -d ' ')
      local avg_tokens=$((total_tokens / (agent_count > 0 ? agent_count : 1)))
      local total_lines=$(jq -r "select(.mode==\"$mode\") | .result_lines" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
      local compactions=$(jq -r "select(.mode==\"$mode\") | .compacted" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)

      eval "${mode}_total_tokens=$total_tokens"
      eval "${mode}_total_cost=$total_cost"
      eval "${mode}_max_duration=$max_duration"
      eval "${mode}_avg_tokens=$avg_tokens"
      eval "${mode}_total_lines=$total_lines"
      eval "${mode}_compactions=$compactions"
    done

    # Calculate deltas
    calc_delta() {
      local baseline=$1
      local canopy=$2
      if [ "$baseline" = "0" ] || [ -z "$baseline" ]; then
        echo "N/A"
        return
      fi
      echo "scale=0; ($canopy - $baseline) * 100 / $baseline" | bc 2>/dev/null || echo "N/A"
    }

    local delta_tokens=$(calc_delta "$baseline_total_tokens" "$canopy_total_tokens")
    local delta_cost=$(calc_delta "${baseline_total_cost%.*}1" "${canopy_total_cost%.*}1" 2>/dev/null || echo "N/A")

    cat >> "$summary_file" << EOF
| Metric | Baseline | Canopy | Delta |
|--------|----------|--------|-------|
| Max agent time | ${baseline_max_duration}s | ${canopy_max_duration}s | |
| Total tokens | $baseline_total_tokens | $canopy_total_tokens | ${delta_tokens}% |
| Total cost | \$$baseline_total_cost | \$$canopy_total_cost | |
| Avg tokens/agent | $baseline_avg_tokens | $canopy_avg_tokens | |
| Total output lines | $baseline_total_lines | $canopy_total_lines | |
| Compactions | $baseline_compactions | $canopy_compactions | |
EOF
  fi

  echo ""
  echo "=============================================="
  echo "  Summary"
  echo "=============================================="
  cat "$summary_file"
  echo ""
  echo "Results saved to: $OUTPUT_DIR"
}

# Main execution
if [ -z "$MODE" ] || [ "$MODE" = "baseline" ]; then
  run_mode "baseline"
fi

if [ -z "$MODE" ] || [ "$MODE" = "canopy" ]; then
  run_mode "canopy"
fi

generate_summary
