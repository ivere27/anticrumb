# `ts-grammars`

Internal workspace crate that builds all 14 Tree-sitter grammars for `anticrumb` with **zlib-compressed LR parse tables** generated automatically at **build time** — with **zero vendored C sources or `.zlib` binaries committed to git**.

## Why

Tree-sitter's generated `parser.c` files embed two large, mostly-zero `uint16_t` LR state tables (`ts_parse_table` and `ts_small_parse_table`). Across 14 bundled languages, those two tables alone account for **16.39 MiB (87.7%)** of uncompressed grammar `.rodata`.

By declaring the upstream `tree-sitter-*` crates solely under `[build-dependencies]`:
- **Nothing is vendored into git:** `ts-grammars/` contains only 4 small text files (`Cargo.toml`, `build.rs`, `src/lib.rs`, `README.md`). Upgrading a grammar never bloats `.git` history with binary diffs.
- **Uncompressed binary drops by ~15.4 MiB:** `build.rs` extracts and zlib-compresses the two tables (`16.39 MiB → 1.02 MiB`, `-93.8%`) into `OUT_DIR` and compiles a table-stripped `parser_slim.c` for the target. The full `[build-dependencies]` crates are never linked into the final `anticrumb` executable.
- **Lazy RAM & zero runtime parse slowdown:** Each language decompresses its table once on first use via `OnceLock` (a few milliseconds). Unused languages consume zero RAM; once loaded, parsing runs at 100% native speed.

## How It Works

### 1. Build Time (`build.rs`)

1. **Extract & compress tables directly from host-linked `TSLanguage`:**
   - Because each upstream `tree-sitter-*` crate is a `[build-dependencies]` dependency, Cargo downloads, verifies (`Cargo.lock`), and links it into the host build script automatically.
   - `build.rs` reads `ts_parse_table` (`large_state_count * symbol_count` `u16` elements) and `ts_small_parse_table` (exact length computed by walking `small_parse_table_map`) directly from the ABI-13/14/15 `TSLanguage` prefix, encodes them in little-endian, and writes `OUT_DIR/<lang>_tables.zlib`.
2. **Compile slimmed target C parser:**
   - Locates each crate's unpacked source directory from `Cargo.lock` + `$CARGO_HOME/registry/src/*` (with `$HOME/.cargo`, `$USERPROFILE/.cargo`, and `vendor/` fallbacks).
   - Streams `src/parser.c` into `OUT_DIR/<lang>_parser_slim.c` with `ts_parse_table` and `ts_small_parse_table` replaced by 1-element dummy stubs and an ABI-safe 6-line C helper `<sym>_with_tables(parse_table, small_parse_table)` appended.
   - Compiles `OUT_DIR/<lang>_parser_slim.c` plus the crate's original `src/scanner.c` (if any) via `cc::Build` for the target triple (supporting Linux, Windows, and macOS cross-compilation).
3. **Generate Rust bindings:**
   - Emits `OUT_DIR/grammars_gen.rs` with one `define_grammar!(...)` invocation per language, included by `src/lib.rs`.

### 2. Runtime (`src/lib.rs`)

- Exposes `ts_grammars::<lang>::LANGUAGE: tree_sitter_language::LanguageFn`.
- On first use, `OnceLock<SyncConstPtr>` decompresses `include_bytes!(concat!(env!("OUT_DIR"), "/<lang>_tables.zlib"))` into aligned `Box<[u16]>` buffers (converting LE bytes safely on any endianness), permanently leaks both via `Box::leak` before passing raw pointers to C, and calls `<sym>_with_tables(pt.as_ptr(), spt.as_ptr())`.
- Because `<sym>_with_tables` lives inside each grammar's own C translation unit, it copies the exact ABI 14 or ABI 15 `TSLanguage` struct without duplicating C struct definitions in Rust, patches only `.parse_table` and `.small_parse_table`, and returns a `&'static TSLanguage`.
- `tree-sitter` 0.25's `ts_language_copy` / `ts_language_delete` never frees non-WASM languages and accesses both tables read-only.

## `tables.zlib` Wire Format (in `OUT_DIR`)

| Offset | Size | Field |
| :--- | :--- | :--- |
| `0` | `4 B` | `pt_len: u32` (LE) — `u16` element count of `ts_parse_table` |
| `4` | `4 B` | `spt_len: u32` (LE) — `u16` element count of `ts_small_parse_table` |
| `8` | `pt_len * 2 B` | `ts_parse_table` elements (`u16` LE) |
| `8 + pt_len * 2` | `spt_len * 2 B` | `ts_small_parse_table` elements (`u16` LE) |

## Updating or Adding a Grammar

### Updating an existing grammar
1. Bump its version under `[build-dependencies]` in [`Cargo.toml`](Cargo.toml).
2. Run `cargo test -p ts-grammars` (updates `Cargo.lock` and verifies clean parsing), then `cargo run --locked -q -p xtask -- check` and `cargo run --locked -q -p xtask -- test`.

### Adding a new grammar
1. Add the upstream crate under `[build-dependencies]` in [`Cargo.toml`](Cargo.toml).
2. Add a `GrammarSpec` entry to `GRAMMARS` in [`build.rs`](build.rs) (`name`, `crate_name`, `src_subpath`, `c_symbol`, `has_scanner`, `language_fn`).
   - Note: if the grammar uses a C++ scanner (`scanner.cc`), enable `.cpp(true)` for that grammar in `build.rs`.
3. Add a clean-parse sample to `all_14_compressed_grammars_parse_cleanly` in [`src/lib.rs`](src/lib.rs).
4. Wire the new language into `anticrumb`:
   - `src/tree_sitter_diff/extensions.json`
   - `SupportedLanguage` and AST handlers in `src/tree_sitter_diff/` (`tree_sitter_diff.rs`, `signature.rs`, `ast_compact.rs`, `test_scope.rs`, `hybrid.rs`, `diff_expand.rs`, `tests.rs`).
5. Verify:
   ```sh
   cargo run --locked -q -p xtask -- check
   cargo run --locked -q -p xtask -- test
   cargo check --no-default-features
   ```

## Notes & Trade-offs

- **Clean build cost:** On a cold Cargo cache, `[build-dependencies]` compiles the upstream `tree-sitter-*` crates once for the host before `build.rs` runs. Incremental builds and Docker builds with `/usr/local/cargo/registry` + `/src/target` cache mounts reuse those artifacts.
- **Registry source lookup & CI cache compatibility:** `build.rs` locates upstream `src/parser.c` via `Cargo.lock` + `$CARGO_HOME/registry/src/*` (or `vendor/`), and automatically falls back to extracting C/C++ source and header files (`.c`/`.h`/`.cc`/`.cpp`/`.cxx`/`.hh`/`.hpp`/`.hxx`) from `$CARGO_HOME/registry/cache/*/*.crate` into `OUT_DIR` when CI caches like `Swatinem/rust-cache@v2` restore `.crate` archives without `registry/src/`. Standard local Cargo installs, GitHub Actions runners, and `Dockerfile` (`CARGO_HOME=/usr/local/cargo`) all work automatically.
