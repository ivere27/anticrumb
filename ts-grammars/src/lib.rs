//! Build-time compressed Tree-sitter C grammars whose `ts_parse_table` and
//! `ts_small_parse_table` arrays are zlib-compressed into `OUT_DIR` during build
//! and lazily decompressed on first use at runtime.

use flate2::read::ZlibDecoder;
use std::io::Read;
use std::sync::OnceLock;
use tree_sitter_language::LanguageFn;

#[repr(transparent)]
struct SyncConstPtr(*const ());

// SAFETY: The pointer refers to a statically allocated, read-only `TSLanguage`
// initialized exactly once inside `OnceLock::get_or_init`.
unsafe impl Send for SyncConstPtr {}
unsafe impl Sync for SyncConstPtr {}

fn decompress_tables(compressed: &[u8]) -> (Box<[u16]>, Box<[u16]>) {
    let mut decoder = ZlibDecoder::new(compressed);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .expect("bundled tree-sitter parse table zlib stream must be valid");
    assert!(raw.len() >= 8, "truncated tree-sitter parse table header");

    let pt_len = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
    let spt_len = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
    let pt_bytes_len = pt_len
        .checked_mul(2)
        .expect("tree-sitter parse_table length overflow");
    let spt_bytes_len = spt_len
        .checked_mul(2)
        .expect("tree-sitter small_parse_table length overflow");
    let expected = 8usize
        .checked_add(pt_bytes_len)
        .and_then(|n| n.checked_add(spt_bytes_len))
        .expect("tree-sitter table byte length overflow");
    assert_eq!(
        raw.len(),
        expected,
        "corrupt tree-sitter parse table payload size"
    );

    let parse_table: Box<[u16]> = raw[8..8 + pt_bytes_len]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    let small_parse_table: Box<[u16]> = raw[8 + pt_bytes_len..]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    (parse_table, small_parse_table)
}

macro_rules! define_grammar {
    ($mod_name:ident, $c_with_tables:ident, $tables_rel:literal) => {
        pub mod $mod_name {
            use super::{decompress_tables, LanguageFn, OnceLock, SyncConstPtr};

            extern "C" {
                fn $c_with_tables(
                    parse_table: *const u16,
                    small_parse_table: *const u16,
                ) -> *const ();
            }

            static LOADED: OnceLock<SyncConstPtr> = OnceLock::new();
            static COMPRESSED_TABLES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), $tables_rel));

            unsafe extern "C" fn load_language() -> *const () {
                let loaded = LOADED.get_or_init(|| {
                    let (parse_table, small_parse_table) = decompress_tables(COMPRESSED_TABLES);
                    // Permanently leak both tables before exposing raw pointers to C so no
                    // `Box` move invalidates the pointers under Stacked Borrows / Tree Borrows.
                    let parse_table: &'static [u16] = Box::leak(parse_table);
                    let small_parse_table: &'static [u16] = Box::leak(small_parse_table);
                    let ptr =
                        unsafe { $c_with_tables(parse_table.as_ptr(), small_parse_table.as_ptr()) };
                    assert!(
                        !ptr.is_null(),
                        "tree-sitter grammar loader returned null TSLanguage"
                    );
                    SyncConstPtr(ptr)
                });
                loaded.0
            }

            pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(load_language) };
        }
    };
}

include!(concat!(env!("OUT_DIR"), "/grammars_gen.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn assert_parses_clean(name: &str, lang: LanguageFn, src: &str) {
        let mut parser = Parser::new();
        parser
            .set_language(&lang.into())
            .unwrap_or_else(|e| panic!("failed to load {name}: {e}"));
        let tree = parser
            .parse(src, None)
            .unwrap_or_else(|| panic!("{name} returned None tree"));
        assert!(
            !tree.root_node().has_error(),
            "{name} failed to parse sample cleanly: {}",
            tree.root_node().to_sexp()
        );
    }

    #[test]
    fn all_14_compressed_grammars_parse_cleanly() {
        let samples: &[(&str, LanguageFn, &str)] = &[
            (
                "rust",
                rust::LANGUAGE,
                "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            ),
            (
                "python",
                python::LANGUAGE,
                "def add(a: int, b: int) -> int:\n    return a + b\n",
            ),
            (
                "javascript",
                javascript::LANGUAGE,
                "export function add(a, b) { return a + b; }\n",
            ),
            (
                "typescript",
                typescript::LANGUAGE,
                "export function add(a: number, b: number): number { return a + b; }\n",
            ),
            (
                "tsx",
                tsx::LANGUAGE,
                "export const Badge = ({ label }: { label: string }) => <span>{label}</span>;\n",
            ),
            (
                "go",
                go::LANGUAGE,
                "package main\nfunc add(a int, b int) int { return a + b }\n",
            ),
            (
                "c",
                c::LANGUAGE,
                "int add(int a, int b) { return a + b; }\n",
            ),
            (
                "cpp",
                cpp::LANGUAGE,
                "template <typename T> T add(T a, T b) { return a + b; }\n",
            ),
            (
                "java",
                java::LANGUAGE,
                "class Calc { int add(int a, int b) { return a + b; } }\n",
            ),
            (
                "csharp",
                csharp::LANGUAGE,
                "public class Calc { public int Add(int a, int b) => a + b; }\n",
            ),
            ("bash", bash::LANGUAGE, "add() { echo $(($1 + $2)); }\n"),
            (
                "ruby",
                ruby::LANGUAGE,
                "class Calc\n  def add(a, b)\n    a + b\n  end\nend\n",
            ),
            (
                "json",
                json::LANGUAGE,
                "{\"ok\": true, \"items\": [1, 2, 3]}\n",
            ),
            (
                "dart",
                dart::LANGUAGE,
                "import 'dart:async';\n\
                 sealed class Result<T> { const Result(); }\n\
                 final class Ok<T> extends Result<T> {\n\
                   final T value;\n\
                   const Ok(this.value);\n\
                 }\n\
                 Future<int> add(int a, {int b = 1}) async => a + b;\n",
            ),
        ];

        for &(name, lang, src) in samples {
            assert_parses_clean(name, lang, src);
        }
    }

    #[test]
    fn concurrent_grammar_initialization_is_race_free() {
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    assert_parses_clean(
                        "dart",
                        dart::LANGUAGE,
                        "class Foo { int x = 1; int get y => x + 1; }\n",
                    );
                    assert_parses_clean(
                        "csharp",
                        csharp::LANGUAGE,
                        "public record Item(int Id, string Name);\n",
                    );
                });
            }
        });
    }
}
