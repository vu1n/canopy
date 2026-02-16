# Canopy Design Anchors (Living Document)

Reviewed: 2026-02-15
Owner: Canopy maintainers
Status: Active working reference

## Purpose

This document is the anti-drift reference for Canopy retrieval architecture.

Goals:
- Capture the core ideas from key inspirations.
- Translate them into Canopy-specific guardrails.
- Require explicit reasoning when we intentionally diverge.

Non-goal:
- This is not a benchmark report. See `docs/benchmarking.md` for measurement protocol.

## Source References

- Recursive Language Models repository: <https://github.com/alexzhang13/rlm>
- Recursive Language Models paper (arXiv:2512.24601): <https://arxiv.org/abs/2512.24601>
- Matryoshka repository: <https://github.com/yogthos/Matryoshka>
- MiniMax Forge article: <https://www.minimax.io/news/forge-scalable-agent-rl-framework-and-algorithm>

## Distilled Concepts

### 1) Externalized Context, Not Prompt Bloat

From RLM and Matryoshka:
- Long context should live in an external environment/state.
- The model should operate on views/snippets/pointers, not repeatedly ingest full corpora.

Canopy implication:
- Handles and server-side state are first-class.
- Returning large raw payloads each turn is architectural debt.

### 2) Reasoning Over Operations, Not Raw Dumps

From RLM and Matryoshka:
- The model should choose operations (search/filter/refine/summarize) over external state.
- Constrained or typed operations reduce entropy and failure rate.

Canopy implication:
- Prefer high-level retrieval operators over manual query-expand loops.
- Tool outputs should bias toward compact evidence, not exhaustive lists.

### 3) Move Work Server-Side

From RLM, Matryoshka, and Forge:
- Compute-heavy steps should execute in the environment/server.
- The model should receive distilled results and citations.

Canopy implication:
- Ranking, dedupe, clustering, novelty checks, and evidence selection should happen server-side.
- Model context should carry conclusions and references, not intermediate bulk data.

### 4) Optimize for Information Gain Per Turn

From RLM and Forge:
- More steps are fine if each step adds meaningful signal.
- Long-horizon loops need convergence pressure based on usefulness, not arbitrary step counts.

Canopy implication:
- Convergence should use novelty and evidence-coverage signals.
- If new calls return low incremental value, pivot to synthesis automatically.

### 5) Latency Matters, But Transport Is Secondary

From Forge and practical Canopy behavior:
- Latency-aware execution is important in production.
- In many failure cases, context/token churn dominates transport overhead.

Canopy implication:
- HTTP vs WebSocket is a second-order optimization unless payload semantics are fixed.
- First fix back-and-forth volume and result compactness.

### 6) Treat Context Management as a First-Class Action

From Forge:
- Context-management policy should be part of the loop, not an afterthought.

Canopy implication:
- Explicit state transitions (result sets, checkpoints, summaries) should be supported natively.
- Avoid implicit growth of conversational context through repeated raw tool transcripts.

## Canopy Guardrails

Any retrieval/API change should satisfy these by default.

1. Pointer-first:
- Return identifiers and compact previews first.
- Expand/materialize only when strictly needed for final synthesis.

2. Server-compute-first:
- Do expensive filtering/ranking/aggregation in service/runtime.
- Avoid sending full candidate sets to the model when a ranked evidence pack is sufficient.

3. Convergence-aware:
- Use novelty and evidence coverage as stop/continue signals.
- Avoid uncontrolled explore loops with low marginal utility.

4. Deterministic citation path:
- Every synthesized claim should map to concrete file-path evidence.
- Retrieval APIs should preserve traceability from summary to source handles.

5. Transport-agnostic correctness:
- Behavior and quality should not depend on local vs remote transport.
- If cloud latency rises, requests should still remain coarse-grained and efficient.

## Drift Signals (Watchlist)

We are drifting if one or more are true:
- Agents frequently hit max turns with `No result`.
- Effective tokens rise sharply while grounded output quality drops.
- Query/expand counts grow without proportional increase in unique evidence.
- Tool payloads repeatedly include large overlapping data already seen in prior turns.
- Service mode underperforms local mode despite hot index and low query latency.

## Decision Framework For Divergence

When changing behavior against these anchors, document:
- Problem statement.
- Which anchor is being bent or violated.
- Why alternatives were rejected.
- Expected upside and explicit downside.
- Exit criteria and rollback trigger.

Use this template:

```md
### Drift Record: <short title>
- Date:
- Owner:
- Related PR:
- Anchor impacted:
- Motivation:
- Change:
- Expected benefit:
- Risk:
- Measurement plan:
- Rollback condition:
- Outcome:
```

## Current Gaps To Prioritize

1. Tool-loop payload economics:
- Even with handle architecture, repeated query/expand still bloats effective context.

2. Missing high-level retrieval operators:
- The model still orchestrates too much low-level plumbing.

3. Weak automatic convergence:
- Current behavior can continue exploring past useful evidence collection.

## Near-Term Direction

1. Introduce stateful result-set operations:
- Query once, refine/rank/dedupe server-side, then fetch bounded evidence packs.

2. Default compact responses:
- Return fewer fields and tighter previews unless detailed mode is requested.

3. Add convergence policy:
- Continue exploration only when novelty/coverage gain exceeds threshold.

4. Keep this document current:
- Update this file whenever behavior intentionally diverges from these anchors.
