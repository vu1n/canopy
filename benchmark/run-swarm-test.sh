#!/usr/bin/env bash
set -e

# Swarm Benchmark: N concurrent agents, baseline vs canopy vs canopy-service
# Measures: token economy, speed, quality across parallel agents

REPO="${1:?Usage: $0 /path/to/repo}"
AGENTS="${AGENTS:-5}"
MODE="${MODE:-}"  # "" = all (baseline + canopy + canopy-service), "baseline", "canopy", "canopy-service", "compare"
COMPARE_MODES="${COMPARE_MODES:-baseline canopy-service}"  # used when MODE=compare
MODEL="${MODEL:-}"
MAX_TURNS="${MAX_TURNS:-15}"
DATE=$(date +%Y%m%d-%H%M%S)
OUTPUT_DIR="${OUTPUT_DIR:-benchmark/results/swarm-$DATE}"
SERVICE_PORT="${SERVICE_PORT:-3099}"
INDEX_TIMEOUT="${INDEX_TIMEOUT:-600}"  # seconds to wait for indexing (default 10 min)

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
if [ "${MODE:-}" = "compare" ]; then
  echo "Compare:     $COMPARE_MODES"
fi
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
    if [ $retries -gt "$INDEX_TIMEOUT" ]; then
      echo " TIMEOUT (${INDEX_TIMEOUT}s)"
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
    local queries expands
    queries=$(jq -r '.performance.queries // 0' "$metrics_file" 2>/dev/null || echo "0")
    expands=$(jq -r '.performance.expands // 0' "$metrics_file" 2>/dev/null || echo "0")
    if [ "${queries:-0}" -eq 0 ] && [ "${expands:-0}" -eq 0 ]; then
      echo "  WARNING: canopy-service recorded 0 /query and 0 /expand calls (service likely not used by agents)"
    fi
  fi
}

fetch_local_feedback_metrics() {
  local mode="$1"
  local db_file="$REPO/.canopy/feedback.db"
  local out_file="$OUTPUT_DIR/$mode/local-feedback-metrics.json"

  if [ ! -f "$db_file" ]; then
    return 0
  fi

  if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "  WARNING: sqlite3 not found; skipping local feedback metrics for $mode"
    return 0
  fi

  local queries expands auto_expands manual_expands
  queries=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM query_events;" 2>/dev/null || echo "0")
  expands=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM expand_events;" 2>/dev/null || echo "0")
  auto_expands=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM expand_events WHERE auto_expanded=1;" 2>/dev/null || echo "0")
  manual_expands=$(sqlite3 "$db_file" "SELECT COUNT(*) FROM expand_events WHERE auto_expanded=0;" 2>/dev/null || echo "0")

  jq -n \
    --arg mode "$mode" \
    --arg db "$db_file" \
    --argjson queries "${queries:-0}" \
    --argjson expands "${expands:-0}" \
    --argjson auto_expands "${auto_expands:-0}" \
    --argjson manual_expands "${manual_expands:-0}" \
    '{
      mode: $mode,
      db_path: $db,
      query_events: $queries,
      expand_events: $expands,
      auto_expand_events: $auto_expands,
      manual_expand_events: $manual_expands
    }' > "$out_file"

  echo "  Local feedback metrics saved to $out_file"
}

run_agent() {
  local mode=$1
  local agent_num=$2
  local task=$3
  local mode_dir="$OUTPUT_DIR/$mode"
  local json_file="$mode_dir/agent-${agent_num}.json"
  local md_file="$mode_dir/agent-${agent_num}.md"
  local stderr_file="$mode_dir/agent-${agent_num}.stderr.log"

  mkdir -p "$mode_dir"

  # For canopy modes, prepend instruction to use canopy MCP tools
  local effective_task="$task"
  if [ "$mode" = "canopy" ] || [ "$mode" = "canopy-service" ]; then
    effective_task="You have access to canopy MCP tools for exploring this codebase. You MUST use the canopy MCP tools (mcp__canopy__canopy_query, mcp__canopy__canopy_expand, mcp__canopy__canopy_status) instead of Read/Grep/Glob/Bash to explore code. Start by querying canopy with relevant patterns or symbols, then expand the handles you need. Do NOT use Read, Grep, Glob, or Bash to search the codebase.

Task: $task"
  fi

  local cmd_args=(
    claude
    --dangerously-skip-permissions
    -p "$effective_task"
    --output-format json
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
{"mcpServers":{"canopy":{"command":"$PROJECT_ROOT/target/release/canopy-mcp","args":["--root","$REPO","--service-url","http://127.0.0.1:$SERVICE_PORT"]}}}
MCPEOF
    )
    cmd_args+=(--mcp-config "$mcp_config")
  fi

  # Run agent from repo root; capture stderr for debugging instead of dropping it.
  local cmd_status=0
  if (cd "$REPO" && "${cmd_args[@]}") > "$json_file" 2> "$stderr_file"; then
    cmd_status=0
  else
    cmd_status=$?
  fi

  # Ensure downstream jq parsing has valid JSON even on command failure.
  if [ ! -s "$json_file" ]; then
    jq -n \
      --arg msg "Agent command failed or produced no JSON output. See $(basename "$stderr_file")." \
      '{result:$msg,duration_ms:0,num_turns:0,usage:{input_tokens:0,cache_read_input_tokens:0,cache_creation_input_tokens:0,output_tokens:0},total_cost_usd:0}' \
      > "$json_file"
  fi

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
  # "Reported tokens" match API billing-style totals (exclude cache reads).
  # "Effective tokens" include cache reads to show total context consumed.
  local total_tokens=$((input_tokens + cache_create + output_tokens))
  local effective_tokens=$((input_tokens + cache_create + cache_read + output_tokens))
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
- **Reported tokens (input + cache_create + output):** $total_tokens
- **Effective tokens (reported + cache_read):** $effective_tokens
- **Cost:** \$${cost}
- **Result lines:** $result_lines
- **Compacted:** $compacted
HEREDOC

  # Append to metrics.jsonl
  echo "{\"mode\":\"$mode\",\"agent\":$agent_num,\"task\":$(echo "$task" | jq -R .),\"duration_ms\":$duration_ms,\"duration_s\":$duration_s,\"num_turns\":$num_turns,\"input_tokens\":$input_tokens,\"cache_read\":$cache_read,\"cache_create\":$cache_create,\"output_tokens\":$output_tokens,\"total_tokens\":$total_tokens,\"effective_tokens\":$effective_tokens,\"cost\":$cost,\"result_lines\":$result_lines,\"compacted\":$compacted}" >> "$OUTPUT_DIR/metrics.jsonl"

  echo "  Agent $agent_num done: ${duration_s}s, ${total_tokens} reported tokens (${effective_tokens} effective), \$$cost"

  if [ "$cmd_status" -ne 0 ]; then
    echo "  Agent $agent_num command failed (exit $cmd_status). See $stderr_file" >&2
    return "$cmd_status"
  fi
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

  # Snapshot local canopy feedback metrics before next mode clears .canopy.
  fetch_local_feedback_metrics "$mode"

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

| Mode | Agent | Task | Duration | Turns | Reported Tokens | Effective Tokens | Cost | Lines |
|------|-------|------|----------|-------|-----------------|------------------|------|-------|
EOF

  if [ -f "$OUTPUT_DIR/metrics.jsonl" ]; then
    while IFS= read -r line; do
      local mode=$(echo "$line" | jq -r '.mode')
      local agent=$(echo "$line" | jq -r '.agent')
      local task=$(echo "$line" | jq -r '.task' | cut -c1-40)
      local duration=$(echo "$line" | jq -r '.duration_s')
      local turns=$(echo "$line" | jq -r '.num_turns')
      local tokens=$(echo "$line" | jq -r '.total_tokens')
      local effective_tokens=$(echo "$line" | jq -r '.effective_tokens // (.input_tokens + .cache_create + .cache_read + .output_tokens)')
      local cost=$(echo "$line" | jq -r '.cost' | xargs printf "%.4f" 2>/dev/null || echo "0")
      local lines=$(echo "$line" | jq -r '.result_lines')
      echo "| $mode | $agent | ${task}... | ${duration}s | $turns | $tokens | $effective_tokens | \$$cost | $lines |" >> "$summary_file"
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

    metric_for_mode() {
      local mode="$1"
      local key="$2"
      case "$key" in
        total_reported_tokens)
          jq -r "select(.mode==\"$mode\") | .total_tokens" "$OUTPUT_DIR/metrics.jsonl" | sum_lines
          ;;
        total_effective_tokens)
          jq -r "select(.mode==\"$mode\") | (.effective_tokens // (.input_tokens + .cache_create + .cache_read + .output_tokens))" "$OUTPUT_DIR/metrics.jsonl" | sum_lines
          ;;
        total_cost)
          jq -r "select(.mode==\"$mode\") | .cost" "$OUTPUT_DIR/metrics.jsonl" | sum_lines
          ;;
        max_duration)
          jq -r "select(.mode==\"$mode\") | .duration_s" "$OUTPUT_DIR/metrics.jsonl" | sort -n | tail -1
          ;;
        avg_effective_tokens)
          local total_tokens
          local agent_count
          total_tokens=$(jq -r "select(.mode==\"$mode\") | (.effective_tokens // (.input_tokens + .cache_create + .cache_read + .output_tokens))" "$OUTPUT_DIR/metrics.jsonl" | sum_lines)
          agent_count=$(jq -r "select(.mode==\"$mode\") | .agent" "$OUTPUT_DIR/metrics.jsonl" | wc -l | tr -d ' ')
          echo $(( total_tokens / (agent_count > 0 ? agent_count : 1) ))
          ;;
        total_cache_read)
          jq -r "select(.mode==\"$mode\") | .cache_read" "$OUTPUT_DIR/metrics.jsonl" | sum_lines
          ;;
        total_lines)
          jq -r "select(.mode==\"$mode\") | .result_lines" "$OUTPUT_DIR/metrics.jsonl" | sum_lines
          ;;
        compactions)
          jq -r "select(.mode==\"$mode\") | .compacted" "$OUTPUT_DIR/metrics.jsonl" | sum_lines
          ;;
        quality_success_rate)
          jq -r "select(.mode==\"$mode\") | [.result_lines, .compacted] | @tsv" "$OUTPUT_DIR/metrics.jsonl" \
            | awk '{total+=1; if ($1>1 && $2==0) ok+=1} END { if (total>0) printf "%.1f", (ok*100.0/total); else print "0.0" }'
          ;;
        quality_avg_lines)
          jq -r "select(.mode==\"$mode\") | .result_lines" "$OUTPUT_DIR/metrics.jsonl" \
            | awk '{s+=$1; n+=1} END { if (n>0) printf "%.1f", (s/n); else print "0.0" }'
          ;;
        quality_avg_turns)
          jq -r "select(.mode==\"$mode\") | .num_turns" "$OUTPUT_DIR/metrics.jsonl" \
            | awk '{s+=$1; n+=1} END { if (n>0) printf "%.1f", (s/n); else print "0.0" }'
          ;;
        quality_null_results)
          jq -r "select(.mode==\"$mode\") | .result_lines" "$OUTPUT_DIR/metrics.jsonl" \
            | awk '{if ($1<=1) c+=1} END {print c+0}'
          ;;
        strict_grounded_rate)
          local total=0
          local grounded=0
          for f in "$OUTPUT_DIR/$mode"/agent-*.json; do
            [ -f "$f" ] || continue
            total=$((total + 1))
            local result
            result=$(jq -r '.result // ""' "$f")
            local refs
            refs=$(printf "%s" "$result" | rg -o '[A-Za-z0-9_./-]+\.(ts|tsx|js|jsx|rs|py|go|md|json|toml|ya?ml|sql|sh)' | wc -l | tr -d ' ')
            if [ "$refs" -ge 3 ]; then
              grounded=$((grounded + 1))
            fi
          done
          awk -v ok="$grounded" -v t="$total" 'BEGIN { if (t>0) printf "%.1f", (ok*100.0/t); else print "0.0" }'
          ;;
        strict_avg_file_refs)
          local total=0
          local refs_sum=0
          for f in "$OUTPUT_DIR/$mode"/agent-*.json; do
            [ -f "$f" ] || continue
            total=$((total + 1))
            local result
            result=$(jq -r '.result // ""' "$f")
            local refs
            refs=$(printf "%s" "$result" | rg -o '[A-Za-z0-9_./-]+\.(ts|tsx|js|jsx|rs|py|go|md|json|toml|ya?ml|sql|sh)' | wc -l | tr -d ' ')
            refs_sum=$((refs_sum + refs))
          done
          awk -v s="$refs_sum" -v t="$total" 'BEGIN { if (t>0) printf "%.1f", (s/t); else print "0.0" }'
          ;;
        strict_structured_rate)
          local total=0
          local structured=0
          for f in "$OUTPUT_DIR/$mode"/agent-*.json; do
            [ -f "$f" ] || continue
            total=$((total + 1))
            local result
            result=$(jq -r '.result // ""' "$f")
            local headings bullets fences
            headings=$(printf "%s\n" "$result" | rg -n '^\s{0,3}#{1,6}\s' | wc -l | tr -d ' ')
            bullets=$(printf "%s\n" "$result" | rg -n '^\s{0,3}[-*]\s' | wc -l | tr -d ' ')
            fences=$(printf "%s" "$result" | rg -o '```' | wc -l | tr -d ' ')
            if [ "$headings" -ge 3 ] && { [ "$bullets" -ge 5 ] || [ "$fences" -ge 1 ]; }; then
              structured=$((structured + 1))
            fi
          done
          awk -v ok="$structured" -v t="$total" 'BEGIN { if (t>0) printf "%.1f", (ok*100.0/t); else print "0.0" }'
          ;;
        service_queries)
          if [ "$mode" = "canopy-service" ] && [ -f "$OUTPUT_DIR/canopy-service/service-metrics.json" ]; then
            jq -r '.performance.queries // 0' "$OUTPUT_DIR/canopy-service/service-metrics.json"
          else
            echo "-"
          fi
          ;;
        service_expands)
          if [ "$mode" = "canopy-service" ] && [ -f "$OUTPUT_DIR/canopy-service/service-metrics.json" ]; then
            jq -r '.performance.expands // 0' "$OUTPUT_DIR/canopy-service/service-metrics.json"
          else
            echo "-"
          fi
          ;;
        local_query_events)
          if [ -f "$OUTPUT_DIR/$mode/local-feedback-metrics.json" ]; then
            jq -r '.query_events // 0' "$OUTPUT_DIR/$mode/local-feedback-metrics.json"
          else
            echo "-"
          fi
          ;;
        local_expand_events)
          if [ -f "$OUTPUT_DIR/$mode/local-feedback-metrics.json" ]; then
            jq -r '.expand_events // 0' "$OUTPUT_DIR/$mode/local-feedback-metrics.json"
          else
            echo "-"
          fi
          ;;
        *)
          echo "0"
          ;;
      esac
    }

    # Build header row dynamically
    local header="| Metric |"
    local separator="|--------|"
    for m in "${modes_ran[@]}"; do
      header="$header $m |"
      separator="$separator------|"
    done
    echo "$header" >> "$summary_file"
    echo "$separator" >> "$summary_file"

    # Build aggregate rows dynamically (Bash 3 compatible: no associative arrays)
    for metric in \
      "Max agent time:max_duration::s" \
      "Total reported tokens:total_reported_tokens::" \
      "Total effective tokens (includes cache read):total_effective_tokens::" \
      "Total cache-read tokens:total_cache_read::" \
      "Total cost:total_cost:\$:" \
      "Avg effective tokens/agent:avg_effective_tokens::" \
      "Total output lines:total_lines::" \
      "Compactions:compactions::"; do
      local label key prefix suffix
      IFS=':' read -r label key prefix suffix <<< "$metric"

      local row="| $label |"
      for m in "${modes_ran[@]}"; do
        local val
        val=$(metric_for_mode "$m" "$key")
        row="$row ${prefix}${val}${suffix} |"
      done
      echo "$row" >> "$summary_file"
    done

    cat >> "$summary_file" << 'QUALITY'

---

## Output Quality (Heuristic)

QUALITY

    echo "$header" >> "$summary_file"
    echo "$separator" >> "$summary_file"

    for metric in \
      "Success rate (non-empty, not compacted):quality_success_rate:%" \
      "Avg output lines:quality_avg_lines:" \
      "Avg turns used:quality_avg_turns:" \
      "Null/empty results:quality_null_results:"; do
      local label=$(echo "$metric" | cut -d: -f1)
      local key=$(echo "$metric" | cut -d: -f2)
      local suffix=$(echo "$metric" | cut -d: -f3)
      local row="| $label |"
      for m in "${modes_ran[@]}"; do
        local val
        val=$(metric_for_mode "$m" "$key")
        row="$row ${val}${suffix} |"
      done
      echo "$row" >> "$summary_file"
    done

    cat >> "$summary_file" << 'QUALITY_STRICT'

---

## Output Quality (Strict Heuristics)

Definitions:
- Grounded output: at least 3 file-path references (e.g. `foo/bar.ts`)
- Structured output: at least 3 headings and at least 5 bullets or 1 code fence

QUALITY_STRICT

    echo "$header" >> "$summary_file"
    echo "$separator" >> "$summary_file"

    for metric in \
      "Grounded outputs:strict_grounded_rate:%" \
      "Avg file refs/output:strict_avg_file_refs:" \
      "Structured outputs:strict_structured_rate:%" \
      "Local query events:local_query_events:" \
      "Local expand events:local_expand_events:" \
      "Service /query calls:service_queries:" \
      "Service /expand calls:service_expands:"; do
      local label=$(echo "$metric" | cut -d: -f1)
      local key=$(echo "$metric" | cut -d: -f2)
      local suffix=$(echo "$metric" | cut -d: -f3)
      local row="| $label |"
      for m in "${modes_ran[@]}"; do
        local val
        val=$(metric_for_mode "$m" "$key")
        row="$row ${val}${suffix} |"
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

resolve_modes() {
  local resolved=()
  case "${MODE:-}" in
    "")
      resolved=("baseline" "canopy" "canopy-service")
      ;;
    "baseline"|"canopy"|"canopy-service")
      resolved=("$MODE")
      ;;
    "compare")
      # shellcheck disable=SC2206
      resolved=($COMPARE_MODES)
      ;;
    *)
      echo "ERROR: Unknown MODE '$MODE'. Expected: baseline|canopy|canopy-service|compare (or empty)." >&2
      exit 2
      ;;
  esac

  for m in "${resolved[@]}"; do
    case "$m" in
      "baseline"|"canopy"|"canopy-service") ;;
      *)
        echo "ERROR: Invalid mode '$m' in mode list." >&2
        exit 2
        ;;
    esac
  done

  echo "${resolved[@]}"
}

# Main execution
read -r -a MODES_TO_RUN <<< "$(resolve_modes)"
for mode in "${MODES_TO_RUN[@]}"; do
  run_mode "$mode"
done

generate_summary
