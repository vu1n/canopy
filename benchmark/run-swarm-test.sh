#!/usr/bin/env bash
set -e

# Swarm Benchmark: N concurrent agents, baseline vs canopy vs canopy-service
# Measures: token economy, speed, quality across parallel agents

REPO="${1:?Usage: $0 /path/to/repo}"
AGENTS="${AGENTS:-5}"
MODE="${MODE:-}"  # "" = all (baseline + canopy + canopy-service), "baseline", "canopy", "canopy-service"
MODEL="${MODEL:-}"
MAX_TURNS="${MAX_TURNS:-15}"
DATE=$(date +%Y%m%d-%H%M%S)
OUTPUT_DIR="${OUTPUT_DIR:-benchmark/results/swarm-$DATE}"
SERVICE_PORT="${SERVICE_PORT:-3099}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TASKS=(
  "Find all authentication-related middleware and explain the auth flow"
  "List all database models/schemas and their relationships"
  "Trace the request lifecycle from HTTP handler to response"
  "Find all background job processors and explain the job queue system"
  "Map the plugin/extension architecture — how are plugins loaded and executed?"
)

SERVICE_PID=""

mkdir -p "$OUTPUT_DIR"

echo "=============================================="
echo "  Canopy Swarm Benchmark"
echo "=============================================="
echo "Repository:  $REPO"
echo "Agents:      $AGENTS"
echo "Max turns:   $MAX_TURNS"
echo "Mode:        ${MODE:-all}"
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
  "mode": "${MODE:-all}",
  "model": "${MODEL:-default}",
  "date": "$DATE",
  "tasks": $(printf '%s\n' "${TASKS[@]}" | jq -R . | jq -s .)
}
EOF

# Build canopy
echo "Building canopy..."
(cd "$PROJECT_ROOT" && cargo build --release -q 2>/dev/null)
echo ""

# ── Service lifecycle ──

start_service() {
  echo "  Starting canopy-service on port $SERVICE_PORT..."
  "$PROJECT_ROOT/target/release/canopy-service" --port "$SERVICE_PORT" &
  SERVICE_PID=$!

  # Wait for service to be ready
  local retries=0
  while ! curl -sf "http://127.0.0.1:$SERVICE_PORT/status" >/dev/null 2>&1; do
    retries=$((retries + 1))
    if [ $retries -gt 30 ]; then
      echo "  ERROR: canopy-service failed to start"
      kill "$SERVICE_PID" 2>/dev/null || true
      return 1
    fi
    sleep 0.5
  done
  echo "  canopy-service ready (pid=$SERVICE_PID)"
}

stop_service() {
  if [ -n "$SERVICE_PID" ]; then
    echo "  Stopping canopy-service (pid=$SERVICE_PID)..."
    kill "$SERVICE_PID" 2>/dev/null || true
    wait "$SERVICE_PID" 2>/dev/null || true
    SERVICE_PID=""
  fi
}

# Clean up service on exit
trap stop_service EXIT

register_and_index_repo() {
  local service_url="http://127.0.0.1:$SERVICE_PORT"

  echo "  Registering repo with service..."
  local add_resp
  add_resp=$(curl -sf -X POST "$service_url/repos/add" \
    -H 'Content-Type: application/json' \
    -d "{\"path\": \"$REPO\"}")

  local repo_id
  repo_id=$(echo "$add_resp" | jq -r '.repo_id')
  echo "  Repo ID: $repo_id"

  echo "  Triggering reindex..."
  curl -sf -X POST "$service_url/reindex" \
    -H 'Content-Type: application/json' \
    -d "{\"repo\": \"$repo_id\"}" >/dev/null

  # Poll until ready
  echo -n "  Waiting for index..."
  local retries=0
  while true; do
    local status
    status=$(curl -sf "$service_url/repos" | jq -r ".[] | select(.repo_id==\"$repo_id\") | .status")
    if [ "$status" = "\"ready\"" ] || [ "$status" = "ready" ]; then
      echo " ready!"
      break
    fi
    if [ "$status" = "\"error\"" ] || [ "$status" = "error" ]; then
      echo " ERROR: indexing failed"
      return 1
    fi
    retries=$((retries + 1))
    if [ $retries -gt 120 ]; then
      echo " TIMEOUT"
      return 1
    fi
    echo -n "."
    sleep 1
  done
}

fetch_service_metrics() {
  local service_url="http://127.0.0.1:$SERVICE_PORT"
  local metrics_file="$OUTPUT_DIR/canopy-service/service-metrics.json"

  if curl -sf "$service_url/metrics" > "$metrics_file" 2>/dev/null; then
    echo "  Service metrics saved to $metrics_file"
  fi
}

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
  elif [ "$mode" = "canopy-service" ]; then
    local mcp_config
    mcp_config=$(cat << MCPEOF
{"mcpServers":{"canopy":{"command":"$PROJECT_ROOT/target/release/canopy-mcp","args":["--root","$REPO"],"env":{"CANOPY_SERVICE_URL":"http://127.0.0.1:$SERVICE_PORT"}}}}
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

  # Start service for canopy-service mode
  if [ "$mode" = "canopy-service" ]; then
    start_service
    register_and_index_repo
  fi

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

  # Fetch service metrics before stopping
  if [ "$mode" = "canopy-service" ]; then
    fetch_service_metrics
    stop_service
  fi

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

  # Generate aggregate comparison — dynamic based on modes that ran
  local modes_ran=()
  if [ -f "$OUTPUT_DIR/metrics.jsonl" ]; then
    while IFS= read -r m; do
      modes_ran+=("$m")
    done < <(jq -r '.mode' "$OUTPUT_DIR/metrics.jsonl" | sort -u)
  fi

  if [ ${#modes_ran[@]} -ge 2 ]; then
    cat >> "$summary_file" << 'DIVIDER'

---

## Aggregate Comparison

DIVIDER

    # Sum helper: reads lines of numbers, outputs their sum via awk
    sum_lines() { awk '{s+=$1} END {print s+0}'; }

    # Build header row dynamically
    local header="| Metric |"
    local separator="|--------|"
    for m in "${modes_ran[@]}"; do
      header="$header $m |"
      separator="$separator------|"
    done
    echo "$header" >> "$summary_file"
    echo "$separator" >> "$summary_file"

    # Calculate aggregates per mode
    declare -A mode_total_tokens mode_total_cost mode_max_duration mode_avg_tokens mode_total_lines mode_compactions
    for m in "${modes_ran[@]}"; do
      mode_total_tokens[$m]=$(jq -r "select(.mode==\"$m\") | .total_tokens" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
      mode_total_cost[$m]=$(jq -r "select(.mode==\"$m\") | .cost" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
      mode_max_duration[$m]=$(jq -r "select(.mode==\"$m\") | .duration_s" "$OUTPUT_DIR/metrics.jsonl" | sort -n | tail -1)
      local agent_count=$(jq -r "select(.mode==\"$m\") | .agent" "$OUTPUT_DIR/metrics.jsonl" | wc -l | tr -d ' ')
      mode_avg_tokens[$m]=$(( ${mode_total_tokens[$m]} / (agent_count > 0 ? agent_count : 1) ))
      mode_total_lines[$m]=$(jq -r "select(.mode==\"$m\") | .result_lines" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
      mode_compactions[$m]=$(jq -r "select(.mode==\"$m\") | .compacted" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
    done

    # Build rows dynamically
    for metric in "Max agent time:max_duration:s" "Total tokens:total_tokens:" "Total cost:total_cost:\$" "Avg tokens/agent:avg_tokens:" "Total output lines:total_lines:" "Compactions:compactions:"; do
      local label=$(echo "$metric" | cut -d: -f1)
      local key=$(echo "$metric" | cut -d: -f2)
      local prefix=$(echo "$metric" | cut -d: -f3)
      local suffix=""
      if [ "$key" = "max_duration" ]; then suffix="s"; fi

      local row="| $label |"
      for m in "${modes_ran[@]}"; do
        local val=""
        case "$key" in
          total_tokens) val="${mode_total_tokens[$m]}" ;;
          total_cost) val="${mode_total_cost[$m]}" ;;
          max_duration) val="${mode_max_duration[$m]}" ;;
          avg_tokens) val="${mode_avg_tokens[$m]}" ;;
          total_lines) val="${mode_total_lines[$m]}" ;;
          compactions) val="${mode_compactions[$m]}" ;;
        esac
        row="$row ${prefix}${val}${suffix} |"
      done
      echo "$row" >> "$summary_file"
    done
  fi

  echo "" >> "$summary_file"
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

if [ -z "$MODE" ] || [ "$MODE" = "canopy-service" ]; then
  run_mode "canopy-service"
fi

generate_summary
