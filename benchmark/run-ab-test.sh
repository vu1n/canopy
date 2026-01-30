#!/usr/bin/env bash
set -e

# A/B Test: Baseline vs Canopy
# Measures: time, token economy, answer quality

REPO="${1:-/Users/vuln/code/n8n}"
OUTPUT_DIR="${2:-/Users/vuln/code/benchmark-results/ab-$(date +%Y%m%d-%H%M%S)}"

mkdir -p "$OUTPUT_DIR"

echo "A/B Test: Baseline vs Canopy"
echo "Repository: $REPO"
echo "Output: $OUTPUT_DIR"
echo ""

# Build canopy
echo "Building canopy..."
(cd /Users/vuln/code/canopy && cargo build --release -q)

run_test() {
    local mode=$1
    local test_name=$2
    local prompt=$3
    local json_file="$OUTPUT_DIR/${mode}-${test_name}.json"
    local output_file="$OUTPUT_DIR/${mode}-${test_name}.md"

    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "[$mode] $test_name"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

    # Clear canopy index for fair comparison
    rm -rf "$REPO/.canopy"

    if [ "$mode" = "baseline" ]; then
        claude --dangerously-skip-permissions -p "$prompt" \
            --output-format json \
            --cwd "$REPO" \
            --max-turns 15 \
            2>/dev/null > "$json_file" || true
    else
        claude --dangerously-skip-permissions -p "$prompt" \
            --output-format json \
            --cwd "$REPO" \
            --max-turns 15 \
            --mcp-config '{"mcpServers":{"canopy":{"command":"/Users/vuln/code/canopy/target/release/canopy-mcp","args":["--root","'"$REPO"'"]}}}' \
            2>/dev/null > "$json_file" || true
    fi

    # Extract metrics
    local duration_ms=$(jq -r '.duration_ms // 0' "$json_file" 2>/dev/null || echo "0")
    local duration_s=$(echo "scale=2; $duration_ms / 1000" | bc)
    local input_tokens=$(jq -r '.usage.input_tokens // 0' "$json_file" 2>/dev/null || echo "0")
    local cache_read=$(jq -r '.usage.cache_read_input_tokens // 0' "$json_file" 2>/dev/null || echo "0")
    local cache_create=$(jq -r '.usage.cache_creation_input_tokens // 0' "$json_file" 2>/dev/null || echo "0")
    local output_tokens=$(jq -r '.usage.output_tokens // 0' "$json_file" 2>/dev/null || echo "0")
    local cost=$(jq -r '.total_cost_usd // 0' "$json_file" 2>/dev/null || echo "0")
    local num_turns=$(jq -r '.num_turns // 0' "$json_file" 2>/dev/null || echo "0")
    local result=$(jq -r '.result // "No result"' "$json_file" 2>/dev/null)

    # Calculate effective tokens (input + cache_create, not double-counting cache_read)
    local effective_input=$((input_tokens + cache_create))
    local total_tokens=$((effective_input + output_tokens))

    # Save readable output
    cat > "$output_file" << HEREDOC
# [$mode] $test_name

## Prompt
$prompt

## Result
$result

---
## Metrics
- **Duration:** ${duration_s}s
- **Turns:** $num_turns
- **Input tokens:** $input_tokens (+ $cache_create cache creation)
- **Cache read:** $cache_read
- **Output tokens:** $output_tokens
- **Effective total:** $total_tokens
- **Cost:** \$${cost}
HEREDOC

    echo "  Duration: ${duration_s}s ($num_turns turns)"
    echo "  Tokens: $input_tokens input + $output_tokens output = $total_tokens total"
    echo "  Cache: $cache_read read, $cache_create created"
    echo "  Cost: \$${cost}"
    echo "  File: $output_file"
    echo ""

    # Append to metrics file
    echo "{\"test\":\"$test_name\",\"mode\":\"$mode\",\"duration_s\":$duration_s,\"turns\":$num_turns,\"input_tokens\":$input_tokens,\"cache_read\":$cache_read,\"cache_create\":$cache_create,\"output_tokens\":$output_tokens,\"total_tokens\":$total_tokens,\"cost\":$cost}" >> "$OUTPUT_DIR/metrics.jsonl"
}

# Test definitions
run_test "baseline" "quick_lookup" "What file contains the AuthController class?"
run_test "canopy" "quick_lookup" "What file contains the AuthController class?"

run_test "baseline" "symbol_search" "List all API route handlers in the codebase"
run_test "canopy" "symbol_search" "List all API route handlers in the codebase"

run_test "baseline" "multi_file" "How does the workflow execution engine work? Trace from trigger to completion."
run_test "canopy" "multi_file" "How does the workflow execution engine work? Trace from trigger to completion."

run_test "baseline" "subsystem" "Explain the credential encryption system - how are credentials stored and retrieved securely?"
run_test "canopy" "subsystem" "Explain the credential encryption system - how are credentials stored and retrieved securely?"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "A/B Test Complete!"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# Generate summary
cat > "$OUTPUT_DIR/summary.md" << 'HEADER'
# A/B Test Summary: Baseline vs Canopy

| Test | Mode | Duration | Turns | Input | Output | Total | Cost |
|------|------|----------|-------|-------|--------|-------|------|
HEADER

if [ -f "$OUTPUT_DIR/metrics.jsonl" ]; then
    while read -r line; do
        test=$(echo "$line" | jq -r '.test')
        mode=$(echo "$line" | jq -r '.mode')
        duration=$(echo "$line" | jq -r '.duration_s')
        turns=$(echo "$line" | jq -r '.turns')
        input=$(echo "$line" | jq -r '.input_tokens')
        output=$(echo "$line" | jq -r '.output_tokens')
        total=$(echo "$line" | jq -r '.total_tokens')
        cost=$(echo "$line" | jq -r '.cost' | xargs printf "%.4f")
        echo "| $test | $mode | ${duration}s | $turns | $input | $output | $total | \$$cost |" >> "$OUTPUT_DIR/summary.md"
    done < "$OUTPUT_DIR/metrics.jsonl"
fi

echo ""
cat "$OUTPUT_DIR/summary.md"
echo ""
echo "Results saved to: $OUTPUT_DIR"
