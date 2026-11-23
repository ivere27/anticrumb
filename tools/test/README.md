# 3-Layer Hybrid `git+ast` Diff — Multi-Arm Ablation & External Baseline Suite

Pure-Rust evaluation harness (`cargo run -p xtask -- bench` / `./tools/test/ab-bench`, zero Python dependency) for benchmarking `@git:diff+` / `@git:staged+` / `@git:<commit>+` across external baselines (`P0`, `P1`, `P2`) and ablation arms (`A`, `B1`, `B2`, `B3`, `B4`).

## Arms (`HybridConfig` & External Baselines)

Hybrid v4 (`B4: all`) is enabled by default in `anticrumb`. The benchmark harness toggles individual layers programmatically via `generate_expanded_diff_with_config` (`HybridConfig`):

| Arm | Mode | Enabled Context / Layers |
|---|---|---|
| **P0** | `git -U3` | Raw standard `git diff/show -U3 -M` unified diff (truncated to budget) |
| **P1** | `git -W` | Raw `git diff/show -W -M` whole-function context (truncated to budget) |
| **P2** | `git -U3 + files` | Raw `-U3` diff + post-change snapshot non-test/non-lockfile full files within budget |
| **A** | `off` | Baseline `@git:diff+` (inline AST expansion + 5-tier compaction only) |
| **B1** | `l1,l2` | Layer 1 (Symbol Index) + Layer 2 (`[sig-only] -U3`, `[clamped]` 200-char params, named `[+ ...]` symbols, ` › ` separator, key decorators) |
| **B2** | `l1,l2,l3_local` | B1 + Layer 3-① (same-file unmodified `struct`/`enum`/`interface`/`type`/`const`) |
| **B3** | `l1,l2,l3_local,l3_cross` | B2 + Layer 3-② (snapshot-safe `git cat-file --batch` 1-hop forward cross-file types, body-stripped signatures, and `// doc:` summaries) |
| **B4 (Default)** | `all` | B3 + Layer 3-③ (snapshot-safe 1-hop reverse callers & consumers across all 10 code languages & UTF-8 identifiers) |

## Quick Start

```sh
# 1. Generate multi-language seeded-bug git fixtures (Rust, TS, Python, Go, Java, C#, C++, Ruby + UTF-8 reverse callers)
./tools/test/ab-bench fixtures --out /tmp/anticrumb_ab/fixtures

# 2. Run Stage 1: Offline multi-arm ablation on real commits + fixtures (dumps patches + JSON/MD tables)
./tools/test/ab-bench offline \
  --commits 575d269,b1bbdab,5aca67c,85c359b,86d4ef9@512,86d4ef9@128 \
  --fixtures-dir /tmp/anticrumb_ab/fixtures \
  --arms P0,P1,P2,A,B1,B2,B3,B4 \
  --out /tmp/anticrumb_ab/run1

# 3. Grade Stage 2: Score JSON review outputs against ground-truth rubrics
./tools/test/ab-bench grade \
  --rubrics /tmp/anticrumb_ab/fixtures/rubrics.json \
  --runs-dir /tmp/anticrumb_ab/run1/llm
```
