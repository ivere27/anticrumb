use flate2::read::GzDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tree_sitter_language::LanguageFn;

/// Common ABI-13/14/15 prefix of `struct TSLanguage` in Tree-sitter's `parser.h`.
#[repr(C)]
struct TsLanguagePrefix {
    abi_version: u32,
    symbol_count: u32,
    alias_count: u32,
    token_count: u32,
    external_token_count: u32,
    state_count: u32,
    large_state_count: u32,
    production_id_count: u32,
    field_count: u32,
    max_alias_sequence_length: u16,
    parse_table: *const u16,
    small_parse_table: *const u16,
    small_parse_table_map: *const u32,
}

struct GrammarSpec {
    name: &'static str,
    crate_name: &'static str,
    src_subpath: &'static str,
    c_symbol: &'static str,
    has_scanner: bool,
    language_fn: LanguageFn,
}

const GRAMMARS: &[GrammarSpec] = &[
    GrammarSpec {
        name: "rust",
        crate_name: "tree-sitter-rust",
        src_subpath: "src",
        c_symbol: "tree_sitter_rust",
        has_scanner: true,
        language_fn: tree_sitter_rust::LANGUAGE,
    },
    GrammarSpec {
        name: "python",
        crate_name: "tree-sitter-python",
        src_subpath: "src",
        c_symbol: "tree_sitter_python",
        has_scanner: true,
        language_fn: tree_sitter_python::LANGUAGE,
    },
    GrammarSpec {
        name: "javascript",
        crate_name: "tree-sitter-javascript",
        src_subpath: "src",
        c_symbol: "tree_sitter_javascript",
        has_scanner: true,
        language_fn: tree_sitter_javascript::LANGUAGE,
    },
    GrammarSpec {
        name: "typescript",
        crate_name: "tree-sitter-typescript",
        src_subpath: "typescript/src",
        c_symbol: "tree_sitter_typescript",
        has_scanner: true,
        language_fn: tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
    },
    GrammarSpec {
        name: "tsx",
        crate_name: "tree-sitter-typescript",
        src_subpath: "tsx/src",
        c_symbol: "tree_sitter_tsx",
        has_scanner: true,
        language_fn: tree_sitter_typescript::LANGUAGE_TSX,
    },
    GrammarSpec {
        name: "go",
        crate_name: "tree-sitter-go",
        src_subpath: "src",
        c_symbol: "tree_sitter_go",
        has_scanner: false,
        language_fn: tree_sitter_go::LANGUAGE,
    },
    GrammarSpec {
        name: "c",
        crate_name: "tree-sitter-c",
        src_subpath: "src",
        c_symbol: "tree_sitter_c",
        has_scanner: false,
        language_fn: tree_sitter_c::LANGUAGE,
    },
    GrammarSpec {
        name: "cpp",
        crate_name: "tree-sitter-cpp",
        src_subpath: "src",
        c_symbol: "tree_sitter_cpp",
        has_scanner: true,
        language_fn: tree_sitter_cpp::LANGUAGE,
    },
    GrammarSpec {
        name: "java",
        crate_name: "tree-sitter-java",
        src_subpath: "src",
        c_symbol: "tree_sitter_java",
        has_scanner: false,
        language_fn: tree_sitter_java::LANGUAGE,
    },
    GrammarSpec {
        name: "csharp",
        crate_name: "tree-sitter-c-sharp",
        src_subpath: "src",
        c_symbol: "tree_sitter_c_sharp",
        has_scanner: true,
        language_fn: tree_sitter_c_sharp::LANGUAGE,
    },
    GrammarSpec {
        name: "bash",
        crate_name: "tree-sitter-bash",
        src_subpath: "src",
        c_symbol: "tree_sitter_bash",
        has_scanner: true,
        language_fn: tree_sitter_bash::LANGUAGE,
    },
    GrammarSpec {
        name: "ruby",
        crate_name: "tree-sitter-ruby",
        src_subpath: "src",
        c_symbol: "tree_sitter_ruby",
        has_scanner: true,
        language_fn: tree_sitter_ruby::LANGUAGE,
    },
    GrammarSpec {
        name: "json",
        crate_name: "tree-sitter-json",
        src_subpath: "src",
        c_symbol: "tree_sitter_json",
        has_scanner: false,
        language_fn: tree_sitter_json::LANGUAGE,
    },
    GrammarSpec {
        name: "dart",
        crate_name: "tree-sitter-dart",
        src_subpath: "src",
        c_symbol: "tree_sitter_dart",
        has_scanner: true,
        language_fn: tree_sitter_dart::LANGUAGE,
    },
];

/// Parses `Cargo.lock` to map crate name -> exact resolved version(s) used by `ts-grammars`.
/// When multiple versions of the same `tree-sitter-*` crate coexist in the workspace lockfile,
/// Cargo disambiguates them in `ts-grammars`'s `dependencies` list as `"<name> <ver>..."`, so
/// we prefer those pinned versions over the global lockfile map.
fn parse_lockfile_versions(lock_path: &Path) -> HashMap<String, Vec<String>> {
    let raw = fs::read_to_string(lock_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", lock_path.display()));
    let mut all_versions: HashMap<String, Vec<String>> = HashMap::new();
    let mut ts_grammars_pinned: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur_name: Option<String> = None;
    let mut cur_ver: Option<String> = None;
    let mut cur_deps: Vec<String> = Vec::new();
    let mut in_deps = false;

    let flush = |name: &mut Option<String>,
                 ver: &mut Option<String>,
                 deps: &mut Vec<String>,
                 all_out: &mut HashMap<String, Vec<String>>,
                 pinned_out: &mut HashMap<String, Vec<String>>| {
        if let (Some(n), Some(v)) = (name.take(), ver.take()) {
            if n == "ts-grammars" {
                for dep in deps.drain(..) {
                    let mut parts = dep.split_whitespace();
                    if let (Some(dep_name), Some(dep_ver)) = (parts.next(), parts.next()) {
                        let entry = pinned_out.entry(dep_name.to_string()).or_default();
                        let dep_ver = dep_ver.to_string();
                        if !entry.contains(&dep_ver) {
                            entry.push(dep_ver);
                        }
                    }
                }
            } else {
                deps.clear();
            }
            let entry = all_out.entry(n).or_default();
            if !entry.contains(&v) {
                entry.push(v);
            }
        } else {
            deps.clear();
        }
    };

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            in_deps = false;
            flush(
                &mut cur_name,
                &mut cur_ver,
                &mut cur_deps,
                &mut all_versions,
                &mut ts_grammars_pinned,
            );
        } else if in_deps {
            if trimmed == "]" {
                in_deps = false;
            } else if let Some(inner) = trimmed
                .trim_end_matches(',')
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
            {
                cur_deps.push(inner.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("name = \"") {
            if let Some(val) = rest.strip_suffix('"') {
                cur_name = Some(val.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("version = \"") {
            if let Some(val) = rest.strip_suffix('"') {
                cur_ver = Some(val.to_string());
            }
        } else if trimmed == "dependencies = [" {
            in_deps = true;
        }
    }
    flush(
        &mut cur_name,
        &mut cur_ver,
        &mut cur_deps,
        &mut all_versions,
        &mut ts_grammars_pinned,
    );

    for (dep_name, pinned_vers) in ts_grammars_pinned {
        all_versions.insert(dep_name, pinned_vers);
    }
    all_versions
}

fn candidate_cargo_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let mut push = |p: PathBuf| {
        if !homes.contains(&p) {
            homes.push(p);
        }
    };
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        push(PathBuf::from(cargo_home));
    }
    if let Some(home) = env::var_os("HOME") {
        push(PathBuf::from(home).join(".cargo"));
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        push(PathBuf::from(profile).join(".cargo"));
    }
    homes
}

fn subdirs_of(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Extracts C/C++ source and header files (`.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx`)
/// from a Cargo `.crate` archive into `dest_root` (used when CI caches like `Swatinem/rust-cache@v2`
/// restore `registry/cache/*.crate` without `registry/src/`).
fn unpack_crate_c_sources(crate_archive: &Path, dest_root: &Path) {
    let file = File::open(crate_archive)
        .unwrap_or_else(|e| panic!("open {}: {e}", crate_archive.display()));
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    for entry in archive
        .entries()
        .unwrap_or_else(|e| panic!("read tar entries {}: {e}", crate_archive.display()))
    {
        let mut entry =
            entry.unwrap_or_else(|e| panic!("tar entry {}: {e}", crate_archive.display()));
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let rel_path = entry
            .path()
            .unwrap_or_else(|e| panic!("tar path {}: {e}", crate_archive.display()))
            .into_owned();
        if rel_path.is_absolute()
            || rel_path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            continue;
        }
        let is_c_or_cpp_source = rel_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx"));
        if !is_c_or_cpp_source {
            continue;
        }
        let target = dest_root.join(&rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("mkdir {}: {e}", parent.display()));
        }
        entry
            .unpack(&target)
            .unwrap_or_else(|e| panic!("unpack {}: {e}", target.display()));
    }
}

fn find_or_unpack_crate_source_dir(
    cargo_homes: &[PathBuf],
    workspace_root: &Path,
    out_dir: &Path,
    crate_name: &str,
    versions: &[String],
    src_subpath: &str,
) -> PathBuf {
    // 1. Check unpacked registry/src directories and workspace vendor/
    let mut src_roots = Vec::new();
    for home in cargo_homes {
        src_roots.extend(subdirs_of(&home.join("registry").join("src")));
    }
    let vendor_dir = workspace_root.join("vendor");
    if vendor_dir.is_dir() {
        src_roots.push(vendor_dir);
    }

    for ver in versions {
        let versioned = format!("{crate_name}-{ver}");
        for root in &src_roots {
            let cand = root.join(&versioned);
            if cand.join(src_subpath).join("parser.c").is_file() {
                return cand;
            }
        }
    }
    for root in &src_roots {
        let unversioned = root.join(crate_name);
        if unversioned.join(src_subpath).join("parser.c").is_file() {
            return unversioned;
        }
    }

    // 2. Fallback: extract C/H sources from registry/cache/*/<crate>-<ver>.crate into OUT_DIR
    let unpack_root = out_dir.join("unpacked_crates");
    for ver in versions {
        let versioned = format!("{crate_name}-{ver}");
        let unpacked = unpack_root.join(&versioned);
        let marker = unpacked.join(".unpacked");
        if marker.is_file() && unpacked.join(src_subpath).join("parser.c").is_file() {
            return unpacked;
        }
        let archive_name = format!("{versioned}.crate");
        for home in cargo_homes {
            for cache_dir in subdirs_of(&home.join("registry").join("cache")) {
                let archive_path = cache_dir.join(&archive_name);
                if archive_path.is_file() {
                    println!("cargo:rerun-if-changed={}", archive_path.display());
                    let staging =
                        unpack_root.join(format!(".staging-{versioned}-{}", std::process::id()));
                    if staging.exists() {
                        let _ = fs::remove_dir_all(&staging);
                    }
                    fs::create_dir_all(&staging)
                        .unwrap_or_else(|e| panic!("mkdir {}: {e}", staging.display()));
                    unpack_crate_c_sources(&archive_path, &staging);
                    let staged_pkg = staging.join(&versioned);
                    if staged_pkg.join(src_subpath).join("parser.c").is_file() {
                        if unpacked.exists() {
                            fs::remove_dir_all(&unpacked).unwrap_or_else(|e| {
                                panic!("remove stale {}: {e}", unpacked.display())
                            });
                        }
                        fs::rename(&staged_pkg, &unpacked).unwrap_or_else(|e| {
                            panic!(
                                "rename {} -> {}: {e}",
                                staged_pkg.display(),
                                unpacked.display()
                            )
                        });
                        let _ = fs::remove_dir_all(&staging);
                        fs::write(&marker, b"ok")
                            .unwrap_or_else(|e| panic!("write {}: {e}", marker.display()));
                        return unpacked;
                    }
                    let _ = fs::remove_dir_all(&staging);
                }
            }
        }
    }

    panic!(
        "could not locate source directory or .crate archive for {crate_name} (versions {versions:?}) \
         under CARGO_HOME candidates {cargo_homes:?} or vendor/; ensure the Cargo registry cache is present"
    );
}

#[derive(Clone, Copy, Debug)]
struct HostTableMeta {
    abi_version: u32,
    symbol_count: u32,
    state_count: u32,
    large_state_count: u32,
}

fn parse_c_u32_define(parser_c: &str, macro_name: &str) -> Option<u32> {
    let prefix = format!("#define {macro_name} ");
    for line in parser_c.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(&prefix) {
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(val) = tok.parse::<u32>() {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Ensures the `parser.c` compiled for the target matches the exact host-linked `TSLanguage`
/// whose LR tables were compressed (guards against version skew when multiple versions exist).
fn verify_parser_c_matches_host_language(name: &str, parser_c: &str, host: &HostTableMeta) {
    let checks = [
        ("LANGUAGE_VERSION", host.abi_version),
        ("SYMBOL_COUNT", host.symbol_count),
        ("STATE_COUNT", host.state_count),
        ("LARGE_STATE_COUNT", host.large_state_count),
    ];
    for (macro_name, expected) in checks {
        let actual = parse_c_u32_define(parser_c, macro_name)
            .unwrap_or_else(|| panic!("{name}: parser.c missing `#define {macro_name}`"));
        assert_eq!(
            actual, expected,
            "{name}: parser.c {macro_name} ({actual}) != host TSLanguage ({expected}); \
             resolved parser.c version does not match build-dependency table source"
        );
    }
}

/// Extracts `ts_parse_table` and `ts_small_parse_table` directly from the host-linked
/// `TSLanguage` pointer and returns `(zlib_compressed_payload, host_meta)`.
fn compress_tables_from_language(name: &str, lang_fn: LanguageFn) -> (Vec<u8>, HostTableMeta) {
    let raw_ptr = unsafe { lang_fn.into_raw()() };
    assert!(
        !raw_ptr.is_null(),
        "{name}: host LanguageFn returned null pointer"
    );
    let lang = unsafe { &*(raw_ptr as *const TsLanguagePrefix) };
    assert!(
        (13..=15).contains(&lang.abi_version),
        "{name}: unexpected TSLanguage ABI version {}",
        lang.abi_version
    );
    assert!(
        lang.state_count >= lang.large_state_count,
        "{name}: state_count ({}) < large_state_count ({})",
        lang.state_count,
        lang.large_state_count
    );
    let meta = HostTableMeta {
        abi_version: lang.abi_version,
        symbol_count: lang.symbol_count,
        state_count: lang.state_count,
        large_state_count: lang.large_state_count,
    };

    let pt_len = (lang.large_state_count as usize)
        .checked_mul(lang.symbol_count as usize)
        .expect("parse_table element count overflow");
    let parse_table: &[u16] = if pt_len == 0 {
        &[]
    } else {
        assert!(!lang.parse_table.is_null(), "{name}: null parse_table");
        unsafe { std::slice::from_raw_parts(lang.parse_table, pt_len) }
    };

    let small_state_count = (lang.state_count - lang.large_state_count) as usize;
    let spt_len = if small_state_count == 0 {
        0usize
    } else {
        assert!(
            !lang.small_parse_table.is_null() && !lang.small_parse_table_map.is_null(),
            "{name}: null small_parse_table or map"
        );
        let map =
            unsafe { std::slice::from_raw_parts(lang.small_parse_table_map, small_state_count) };
        let mut max_end = 0usize;
        for &offset_u32 in map {
            let mut idx = offset_u32 as usize;
            let group_count = unsafe { *lang.small_parse_table.add(idx) } as usize;
            idx += 1;
            for _ in 0..group_count {
                let sym_count = unsafe { *lang.small_parse_table.add(idx + 1) } as usize;
                idx += 2 + sym_count;
            }
            if idx > max_end {
                max_end = idx;
            }
        }
        max_end
    };
    let small_parse_table: &[u16] = if spt_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(lang.small_parse_table, spt_len) }
    };

    let mut payload = Vec::with_capacity(8 + (pt_len + spt_len) * 2);
    payload.extend_from_slice(&(pt_len as u32).to_le_bytes());
    payload.extend_from_slice(&(spt_len as u32).to_le_bytes());
    for &v in parse_table {
        payload.extend_from_slice(&v.to_le_bytes());
    }
    for &v in small_parse_table {
        payload.extend_from_slice(&v.to_le_bytes());
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(&payload)
        .unwrap_or_else(|e| panic!("{name}: zlib write failed: {e}"));
    let compressed = encoder
        .finish()
        .unwrap_or_else(|e| panic!("{name}: zlib finish failed: {e}"));
    (compressed, meta)
}

fn strip_parse_tables_and_append_helper(
    parser_c: &str,
    c_symbol: &str,
    host: &HostTableMeta,
) -> String {
    let mut out = String::with_capacity(parser_c.len() / 6);
    let mut replaced_parse_table = 0usize;
    let mut replaced_small_parse_table = 0usize;
    let mut replaced_public_fn = 0usize;
    let public_fn_prefix = format!("TS_PUBLIC const TSLanguage *{c_symbol}(void)");
    let static_fn_prefix = format!("static const TSLanguage *{c_symbol}(void)");

    enum SkipState {
        None,
        Skipping { brace_depth: usize, saw_open: bool },
    }
    let mut state = SkipState::None;

    for line in parser_c.split_inclusive('\n') {
        match state {
            SkipState::None => {
                if line.starts_with("static const uint16_t ts_parse_table[") {
                    replaced_parse_table += 1;
                    out.push_str("static const uint16_t ts_parse_table[1][1] = {{0}};\n");
                    let mut depth = 0usize;
                    let mut saw_open = false;
                    for b in line.bytes() {
                        if b == b'{' {
                            depth += 1;
                            saw_open = true;
                        } else if b == b'}' {
                            depth = depth.saturating_sub(1);
                        }
                    }
                    if !(saw_open && depth == 0 && line.trim_end().ends_with(';')) {
                        state = SkipState::Skipping {
                            brace_depth: depth,
                            saw_open,
                        };
                    }
                } else if line.starts_with("static const uint16_t ts_small_parse_table[") {
                    replaced_small_parse_table += 1;
                    out.push_str("static const uint16_t ts_small_parse_table[1] = {0};\n");
                    let mut depth = 0usize;
                    let mut saw_open = false;
                    for b in line.bytes() {
                        if b == b'{' {
                            depth += 1;
                            saw_open = true;
                        } else if b == b'}' {
                            depth = depth.saturating_sub(1);
                        }
                    }
                    if !(saw_open && depth == 0 && line.trim_end().ends_with(';')) {
                        state = SkipState::Skipping {
                            brace_depth: depth,
                            saw_open,
                        };
                    }
                } else if let Some(rest) = line.strip_prefix(&public_fn_prefix) {
                    // Keep the dummy-table `tree_sitter_<lang>` internal so only
                    // `{c_symbol}_with_tables` is exported from the slimmed object file.
                    replaced_public_fn += 1;
                    out.push_str(&static_fn_prefix);
                    out.push_str(rest);
                } else {
                    out.push_str(line);
                }
            }
            SkipState::Skipping {
                mut brace_depth,
                mut saw_open,
            } => {
                for b in line.bytes() {
                    if b == b'{' {
                        brace_depth += 1;
                        saw_open = true;
                    } else if b == b'}' {
                        brace_depth = brace_depth.saturating_sub(1);
                    }
                }
                if saw_open && brace_depth == 0 && line.trim_end().ends_with(';') {
                    state = SkipState::None;
                } else {
                    state = SkipState::Skipping {
                        brace_depth,
                        saw_open,
                    };
                }
            }
        }
    }

    assert!(
        matches!(state, SkipState::None),
        "{c_symbol}: unterminated parse table block while stripping parser.c"
    );
    if host.large_state_count > 0 && host.symbol_count > 0 {
        assert_eq!(
            replaced_parse_table, 1,
            "{c_symbol}: expected exactly 1 ts_parse_table"
        );
    } else {
        assert!(
            replaced_parse_table <= 1,
            "{c_symbol}: expected at most 1 ts_parse_table"
        );
    }
    if host.state_count > host.large_state_count {
        assert_eq!(
            replaced_small_parse_table, 1,
            "{c_symbol}: expected exactly 1 ts_small_parse_table"
        );
    } else {
        assert!(
            replaced_small_parse_table <= 1,
            "{c_symbol}: expected at most 1 ts_small_parse_table"
        );
    }
    assert_eq!(
        replaced_public_fn, 1,
        "{c_symbol}: expected exactly 1 `{public_fn_prefix}` definition"
    );

    out.push_str(&format!(
        "\n#ifdef __cplusplus\nextern \"C\" {{\n#endif\n\
         TS_PUBLIC const TSLanguage *{c_symbol}_with_tables(\n\
             const uint16_t *parse_table,\n\
             const uint16_t *small_parse_table) {{\n\
           static TSLanguage language_with_tables;\n\
           language_with_tables = *{c_symbol}();\n\
           language_with_tables.parse_table = parse_table;\n\
           language_with_tables.small_parse_table = small_parse_table;\n\
           return &language_with_tables;\n\
         }}\n\
         #ifdef __cplusplus\n}}\n#endif\n"
    ));

    out
}

fn write_if_changed(path: &Path, bytes: &[u8]) {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return;
    }
    fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent()
        .expect("ts-grammars must live inside workspace root");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let lock_path = workspace_root.join("Cargo.lock");

    // Note: intentionally do NOT emit `cargo:rerun-if-changed=Cargo.lock` — Cargo already
    // recompiles and reruns `build.rs` whenever any `[build-dependencies]` `tree-sitter-*`
    // crate changes in `Cargo.lock`, so unrelated lockfile churn does not force re-compressing
    // and recompiling all 14 grammars.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=CARGO_HOME");
    println!("cargo:rerun-if-env-changed=HOME");
    println!("cargo:rerun-if-env-changed=USERPROFILE");

    let lock_versions = parse_lockfile_versions(&lock_path);
    let cargo_homes = candidate_cargo_homes();
    let is_msvc = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc";

    let mut generated_rs = String::new();

    for spec in GRAMMARS {
        let versions = lock_versions
            .get(spec.crate_name)
            .unwrap_or_else(|| panic!("{} missing from Cargo.lock", spec.crate_name));
        let crate_dir = find_or_unpack_crate_source_dir(
            &cargo_homes,
            workspace_root,
            &out_dir,
            spec.crate_name,
            versions,
            spec.src_subpath,
        );
        let in_out_dir = crate_dir.starts_with(&out_dir);
        let src_dir = crate_dir.join(spec.src_subpath);
        let parser_path = src_dir.join("parser.c");
        if !in_out_dir {
            println!("cargo:rerun-if-changed={}", parser_path.display());
        }

        // 1. Compress LR parse tables directly from host-linked TSLanguage into OUT_DIR
        let (compressed_tables, host_meta) =
            compress_tables_from_language(spec.name, spec.language_fn);
        let tables_out = out_dir.join(format!("{}_tables.zlib", spec.name));
        write_if_changed(&tables_out, &compressed_tables);

        // 2. Verify parser.c matches host TSLanguage, strip parse tables, and compile for target
        let parser_c = fs::read_to_string(&parser_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", parser_path.display()));
        verify_parser_c_matches_host_language(spec.name, &parser_c, &host_meta);
        let parser_slim =
            strip_parse_tables_and_append_helper(&parser_c, spec.c_symbol, &host_meta);
        let slim_out = out_dir.join(format!("{}_parser_slim.c", spec.name));
        write_if_changed(&slim_out, parser_slim.as_bytes());

        let mut cfg = cc::Build::new();
        cfg.std("c11")
            .include(&src_dir)
            .warnings(false)
            .flag_if_supported("-Wno-unused-value")
            .flag_if_supported("-Wno-unused-parameter");
        if is_msvc {
            cfg.flag("-utf-8");
        }
        cfg.file(&slim_out);
        if spec.has_scanner {
            let scanner_path = src_dir.join("scanner.c");
            if !in_out_dir {
                println!("cargo:rerun-if-changed={}", scanner_path.display());
            }
            cfg.file(&scanner_path);
        }
        cfg.compile(&format!("ts-grammar-{}", spec.name));

        generated_rs.push_str(&format!(
            "define_grammar!({}, {}_with_tables, \"/{}_tables.zlib\");\n",
            spec.name, spec.c_symbol, spec.name
        ));
    }

    write_if_changed(&out_dir.join("grammars_gen.rs"), generated_rs.as_bytes());
}
