# 3-Layer Hybrid `git+ast` Diff — A/B & 4-Arm Ablation Test Suite

Pure-Rust evaluation harness (`cargo run -p xtask -- bench` / `./tools/test/ab-bench`, zero Python dependency) for benchmarking `@git:diff+` / `@git:staged+` / `@git:<commit>+` across 4 ablation arms.

## Ablation Arms (`HybridConfig`)

Hybrid v3 (`B3: all`) is enabled by default in `anticrumb`. The benchmark harness toggles individual layers programmatically via `generate_expanded_diff_with_config` (`HybridConfig`):

| Arm | `HybridConfig` Mode | Enabled Layers |
|---|---|---|
| **A** | `off` | Baseline `@git:diff+` (inline AST expansion + 5-tier compaction only) |
| **B1** | `l1,l2` | Layer 1 (Symbol Index) + Layer 2 (`[sig-only] -U3`, `[clamped]` 200-char params, named `[+ ...]` symbols, ` › ` separator, key decorators) |
| **B2** | `l1,l2,l3_local` | B1 + Layer 3-① (same-file unmodified `struct`/`enum`/`interface`/`type`/`const`) |
| **B3 (Default)** | `all` | B2 + Layer 3-② (snapshot-safe `git cat-file --batch` 1-hop cross-file types, body-stripped signatures, and `// doc:` summaries) |

## Quick Start

```sh
# 1. Generate multi-language seeded-bug git fixtures (Rust, TS, Python, Go, Java, C#, C++, Ruby)
./tools/test/ab-bench fixtures --out /tmp/anticrumb_ab/fixtures

# 2. Run Stage 1: Offline 4-arm ablation on real commits + fixtures (dumps patches + JSON/MD tables)
./tools/test/ab-bench offline \
  --commits 575d269,b1bbdab,5aca67c,85c359b,86d4ef9@512,86d4ef9@128 \
  --fixtures-dir /tmp/anticrumb_ab/fixtures \
  --arms A,B1,B2,B3 \
  --out /tmp/anticrumb_ab/run1

# 3. Grade Stage 2: Score JSON review outputs against ground-truth rubrics
./tools/test/ab-bench grade \
  --rubrics /tmp/anticrumb_ab/fixtures/rubrics.json \
  --runs-dir /tmp/anticrumb_ab/run1/llm
```
