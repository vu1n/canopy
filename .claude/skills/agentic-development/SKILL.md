---
name: agentic-development
description: >
  Repo-local best practices for agent-assisted development. Use whenever editing
  or refactoring code, and especially when resolving desloppify findings.
  Optimizes for retrievability, locality, and machine-checkable correctness
  under context limits.
---

# Agentic Development Addendum

## Relationship to Desloppify (important)
- When desloppify is involved: **run `desloppify next` and follow its instructions exactly**.
- This addendum is only a **tie-breaker** for *how* to implement a fix when multiple approaches satisfy `next`.
- Prefer changes that increase strict score **and** reduce future context burden.

## Core goals
- **Locality:** most changes should be understandable from a small bounded slice (module entrypoint + a few related files + tests).
- **Retrievability:** make it easy to find “the 5 files that matter” for a behavior.
- **Verification:** correctness comes from checks (tests/typecheck/lint), not global understanding.

## Tie-breakers (when multiple fixes are plausible)
Prefer the option that:
1. **Reduces context cone** (fewer transitive deps; fewer cross-module edits).
2. **Makes boundaries explicit** (types/schemas/public entrypoints; fewer implicit globals).
3. **Moves logic into deterministic core** (functional core / imperative shell).
4. **Adds/strengthens tests** that lock intent.
5. **Avoids hidden magic** (reflection, deep inheritance, implicit DI wiring, monkeypatching).

Avoid:
- broad refactors bundled with a fix
- clever abstractions that hide control flow/dependencies
- cross-cutting edits touching many unrelated modules

## Preferred patterns
- Modular monolith boundaries by default (internal packages/modules with explicit APIs)
- Composition over inheritance
- Explicit dependency injection (constructors/params)
- Schema/type-first boundaries (esp. cross-module/service edges)
- Idempotent, retry-safe side effects

## Minimal-context workflow
1. Identify the owning module / boundary.
2. Read the module entrypoint + relevant types/schemas + tests.
3. Make the smallest possible change to satisfy `desloppify next`.
4. Run repo checks; fix forward.
5. Update boundary docs if contracts changed.

## Tooling guidance (lightest tool first)
- literal string? → `rg`
- structural change across many files? → `ast-grep` (+ tests)
- find files by name? → `fd`/glob
- unsure where behavior lives / need call-path + top evidence? → `canopy` (if available)