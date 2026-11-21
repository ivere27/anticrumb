use super::*;
use super::{diff_expand::*, signature::*, test_scope::*};
use std::io::Write;
use tree_sitter::Parser;

fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    };
    run(&["init"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "core.autocrlf", "false"]);
}

#[test]
fn test_pure_insertion_old_exp_start_offset_and_git_apply_check() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial = "\
fn calculate_total(base: i32) -> i32 {
    let x = base;
    let y = 10;
    let z = 20;
    x + y + z
}
";
    let file_path = repo.join("calc.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "calc.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Pure insertion of 2 lines after line 3
    let modified = "\
fn calculate_total(base: i32) -> i32 {
    let x = base;
    let extra = 5;
    let factor = 2;
    let y = 10;
    let z = 20;
    x + y + z
}
";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    // Header check: old_exp_start must start at 1, NOT 0
    assert!(
        patch.contains("@@ -1,6 +1,8 @@ fn calculate_total"),
        "Expected header with old_start=1, got:\n{patch}"
    );

    // Verify git apply --check passes against the committed baseline
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "calc.rs"])
        .status()
        .unwrap();

    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git apply --check failed: {stderr}\nPatch:\n{patch}"
    );
}

#[test]
fn test_nested_scope_concurrent_modifications_hunk_ordering() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial = "\
fn outer_fn() {
    let first = 1;
    fn inner_fn() {
        let inner_val = 2;
    }
    let third = 3;
}
";
    let file_path = repo.join("nested.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "nested.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Modify 1: outer (line 2), 2: inner (line 4), 3: outer (line 6)
    let modified = "\
fn outer_fn() {
    let first = 100;
    fn inner_fn() {
        let inner_val = 200;
    }
    let third = 300;
}
";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    // Check that changes appear in chronological order (first, then inner_val, then third)
    let pos_first = patch.find("+    let first = 100;").unwrap();
    let pos_inner = patch.find("+        let inner_val = 200;").unwrap();
    let pos_third = patch.find("+    let third = 300;").unwrap();
    assert!(
        pos_first < pos_inner && pos_inner < pos_third,
        "Hunk order inverted! Expected first < inner < third, got positions {pos_first}, {pos_inner}, {pos_third}\nPatch:\n{patch}"
    );

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "nested.rs"])
        .status()
        .unwrap();

    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git apply --check failed: {stderr}\nPatch:\n{patch}"
    );
}

#[test]
fn test_subdirectory_workspace_non_ascii_and_rename() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let sub_dir = repo.join("sub_module");
    std::fs::create_dir_all(&sub_dir).unwrap();

    let non_ascii_file = sub_dir.join("unicode_café.rs");
    std::fs::write(&non_ascii_file, "fn dummy() {\n    let old = 1;\n}\n").unwrap();

    let rename_old = sub_dir.join("old_name.rs");
    std::fs::write(&rename_old, "fn renamed_func() {\n    let val = 10;\n}\n").unwrap();

    Command::new("git")
        .current_dir(repo)
        .args(["add", "."])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // 1. Modify non-ascii file
    std::fs::write(&non_ascii_file, "fn dummy() {\n    let old = 999;\n}\n").unwrap();

    // 2. Git mv rename with modification
    let rename_new = sub_dir.join("new_name.rs");
    Command::new("git")
        .current_dir(repo)
        .args(["mv", "sub_module/old_name.rs", "sub_module/new_name.rs"])
        .status()
        .unwrap();
    std::fs::write(&rename_new, "fn renamed_func() {\n    let val = 20;\n}\n").unwrap();

    // Run from subdirectory `sub_module` as workspace!
    let mut warnings = Vec::new();
    let unstaged_patch = generate_expanded_diff(&sub_dir, false, &mut warnings).unwrap();
    assert!(
        unstaged_patch.contains("sub_module/unicode_café.rs"),
        "Expected sub_module/unicode_café.rs in patch, got:\n{unstaged_patch}"
    );
    assert!(
        unstaged_patch.contains("sub_module/new_name.rs"),
        "Expected sub_module/new_name.rs in patch, got:\n{unstaged_patch}"
    );

    // Also test staged diff for the rename
    let staged_patch = generate_expanded_diff(&sub_dir, true, &mut warnings).unwrap();
    assert!(
        staged_patch.contains("rename from sub_module/old_name.rs")
            || staged_patch.contains("sub_module/new_name.rs"),
        "Expected rename in staged patch, got:\n{staged_patch}"
    );
}

#[test]
fn test_ast_boundaries_rust_turbofish_java_method_json_pair() {
    // 1. Rust turbofish collect::<Vec<_>>()
    let rust_src = "\
fn process_items(items: &[i32]) -> Vec<i32> {
    let result = items
        .iter()
        .map(|x| x * 2)
        .collect::<Vec<_>>();
    result
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(rust_src, None).unwrap();
    let (start, end, sig) =
        find_enclosing_definition(SupportedLanguage::Rust, &tree.root_node(), 5, 5, rust_src);
    assert_eq!(start, 1);
    assert_eq!(end, 7);
    assert!(sig.as_deref().unwrap().starts_with("fn process_items"));

    // 2. Java method invocation
    let java_src = "\
class UserService {
    public void updateUser(User user) {
        String name = user.getName().trim();
        save(name);
    }
}
";
    let mut parser_java = Parser::new();
    parser_java
        .set_language(&SupportedLanguage::Java.language_fn())
        .unwrap();
    let tree_java = parser_java.parse(java_src, None).unwrap();
    let (j_start, j_end, j_sig) = find_enclosing_definition(
        SupportedLanguage::Java,
        &tree_java.root_node(),
        3,
        3,
        java_src,
    );
    assert_eq!(j_start, 2);
    assert_eq!(j_end, 5);
    assert!(j_sig
        .as_deref()
        .unwrap()
        .contains("public void updateUser(User user)"));

    // 3. JSON pair
    let json_src = "{\n  \"port\": 8080,\n  \"timeout\": 30,\n  \"name\": \"app\"\n}\n";
    let mut parser_json = Parser::new();
    parser_json
        .set_language(&SupportedLanguage::Json.language_fn())
        .unwrap();
    let tree_json = parser_json.parse(json_src, None).unwrap();
    let (js_start, js_end, js_sig) = find_enclosing_definition(
        SupportedLanguage::Json,
        &tree_json.root_node(),
        3,
        3,
        json_src,
    );
    assert_eq!(js_start, 3);
    assert_eq!(js_end, 3);
    assert_eq!(js_sig.as_deref().unwrap(), "\"timeout\": 30,");
}

#[test]
fn test_rust_attribute_item_sibling_does_not_expand_to_enclosing_mod() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial_rust = "\
mod tests {
    #[test]
    fn test_first() {
        assert_eq!(1, 1);
    }

    #[test]
    fn test_second() {
        assert_eq!(2, 2);
    }
}
";
    let file_path = repo.join("test_mod.rs");
    std::fs::write(&file_path, initial_rust).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "test_mod.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Add a new test with #[test] attribute between test_first and test_second
    let modified_rust = "\
mod tests {
    #[test]
    fn test_first() {
        assert_eq!(1, 1);
    }

    #[test]
    fn test_inserted() {
        assert_eq!(99, 99);
    }

    #[test]
    fn test_second() {
        assert_eq!(2, 2);
    }
}
";
    std::fs::write(&file_path, modified_rust).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    // Must NOT expand to the enclosing `mod tests {`
    assert!(
        !patch.contains("@@ -1,"),
        "Patch must not start at line 1 of mod tests! Got:\n{patch}"
    );
    assert!(
        patch.contains("fn test_inserted"),
        "Patch must contain the newly inserted test. Got:\n{patch}"
    );

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "test_mod.rs"])
        .status()
        .unwrap();

    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git apply --check failed: {stderr}\nPatch:\n{patch}"
    );
}

#[test]
fn test_js_callback_vs_top_level_arrow_function() {
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::JavaScript.language_fn())
        .unwrap();

    // 1. Inline callback inside function should expand to the enclosing function
    let js_callback = "\
function processItems(items) {
    const doubled = items.map(x => x * 2);
    return doubled;
}
";
    let tree1 = parser.parse(js_callback, None).unwrap();
    // Line 2: items.map(x => x * 2)
    let (s1, e1, sig1) = find_enclosing_definition(
        SupportedLanguage::JavaScript,
        &tree1.root_node(),
        2,
        2,
        js_callback,
    );
    assert_eq!(s1, 1);
    assert_eq!(e1, 4);
    assert!(sig1.as_deref().unwrap().contains("function processItems"));

    // 2. Assigned arrow function should expand to the variable declaration with identifier
    let js_const_arrow = "\
const handleEvent = (event) => {
    validate(event);
    save(event);
};
";
    let tree2 = parser.parse(js_const_arrow, None).unwrap();
    // Line 2: validate(event);
    let (s2, e2, sig2) = find_enclosing_definition(
        SupportedLanguage::JavaScript,
        &tree2.root_node(),
        2,
        2,
        js_const_arrow,
    );
    assert_eq!(s2, 1);
    assert_eq!(e2, 4);
    assert!(sig2.as_deref().unwrap().starts_with("const handleEvent"));
}

#[test]
fn test_c_struct_usage_inside_function_does_not_block_function_expansion() {
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::C.language_fn())
        .unwrap();

    let c_code = "\
void run_task() {
    struct TaskConfig *cfg = NULL;
    execute(cfg);
}
";
    let tree = parser.parse(c_code, None).unwrap();
    // Line 2 is struct TaskConfig *cfg = NULL;
    let (start, end, sig) =
        find_enclosing_definition(SupportedLanguage::C, &tree.root_node(), 2, 2, c_code);
    assert_eq!(start, 1);
    assert_eq!(end, 4);
    assert!(sig.as_deref().unwrap().contains("void run_task()"));
}

#[test]
fn test_c_struct_field_expansion_and_cpp_namespace() {
    // 1. C struct field edit should expand to the enclosing struct definition
    let mut parser_c = Parser::new();
    parser_c
        .set_language(&SupportedLanguage::C.language_fn())
        .unwrap();
    let c_struct = "\
struct ServerConfig {
    int timeout_sec;
    int port;
};
";
    let tree_c = parser_c.parse(c_struct, None).unwrap();
    // Line 2: int timeout_sec;
    let (s_c, e_c, sig_c) =
        find_enclosing_definition(SupportedLanguage::C, &tree_c.root_node(), 2, 2, c_struct);
    assert_eq!(s_c, 1);
    assert_eq!(e_c, 4);
    assert!(sig_c.as_deref().unwrap().contains("struct ServerConfig"));

    // 2. C++ namespace should NOT expand the entire namespace, but only the modified function
    let mut parser_cpp = Parser::new();
    parser_cpp
        .set_language(&SupportedLanguage::Cpp.language_fn())
        .unwrap();
    let cpp_ns = "\
namespace network {
    void connect() {
        init();
    }
    void disconnect() {
        close();
    }
}
";
    let tree_cpp = parser_cpp.parse(cpp_ns, None).unwrap();
    // Line 3: init();
    let (s_cpp, e_cpp, sig_cpp) =
        find_enclosing_definition(SupportedLanguage::Cpp, &tree_cpp.root_node(), 3, 3, cpp_ns);
    assert_eq!(s_cpp, 2);
    assert_eq!(e_cpp, 4);
    assert!(sig_cpp.as_deref().unwrap().contains("void connect()"));
}

#[test]
fn test_no_newline_at_end_of_file_marker_supported_language() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Case 1: Mid-file modification pulling the last line (without trailing newline) into post_context
    let initial_rust = b"fn run() {\n    let a = 1;\n    let b = 2;\n}"; // No trailing newline
    let file_path = repo.join("no_nl.rs");
    std::fs::write(&file_path, initial_rust).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "no_nl.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Modify line 2 (middle of function)
    let modified_mid = b"fn run() {\n    let a = 99;\n    let b = 2;\n}";
    std::fs::write(&file_path, modified_mid).unwrap();

    let mut warnings = Vec::new();
    let patch_mid = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    assert!(
        patch_mid.contains("\\ No newline at end of file"),
        "Expected '\\ No newline at end of file' in mid-file patch, got:\n{patch_mid}"
    );
    // Ensure no duplicate markers
    let count_mid = patch_mid.matches("\\ No newline at end of file").count();
    assert_eq!(count_mid, 1, "Expected exactly 1 marker, got {count_mid}");

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "no_nl.rs"])
        .status()
        .unwrap();
    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch_mid.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git apply --check failed for mid-file edit: {}\nPatch:\n{patch_mid}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Case 2: Last line itself modified
    let modified_last = b"fn run() {\n    let a = 1;\n    let b = 2;\n} // end";
    std::fs::write(&file_path, modified_last).unwrap();

    let patch_last = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(
        patch_last.contains("\\ No newline at end of file"),
        "Expected '\\ No newline at end of file' in last-line patch, got:\n{patch_last}"
    );
    let count_last = patch_last.matches("\\ No newline at end of file").count();
    assert_eq!(count_last, 2, "Expected 2 markers (removed and added lines) in last-line replacement patch, got {count_last}");

    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "no_nl.rs"])
        .status()
        .unwrap();
    let mut apply_cmd2 = Command::new("git");
    apply_cmd2.current_dir(repo).args(["apply", "--check", "-"]);
    let mut child2 = apply_cmd2
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child2
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch_last.as_bytes())
        .unwrap();
    let output2 = child2.wait_with_output().unwrap();
    assert!(
        output2.status.success(),
        "git apply --check failed for last-line edit: {}\nPatch:\n{patch_last}",
        String::from_utf8_lossy(&output2.stderr)
    );
}

#[test]
fn test_utf8_multibyte_signature_truncation_no_panic() {
    // Multi-byte Unicode characters (emoji, Cyrillic, Latin accented) positioned across the 117-byte boundary
    let long_line = format!(
        "pub fn process_event_{}(data: &str) -> bool {{",
        "✨".repeat(120)
    );
    let lines = vec![long_line.as_str()];
    let sig = extract_signature_from_row(0, &lines);
    assert!(sig.is_some());
    let res = sig.unwrap();
    assert!(res.ends_with("..."));
    assert!(res.chars().count() <= 120);
}

#[test]
fn test_oversized_function_context_clamping_retains_signature() {
    let mut code = String::from("fn giant_function() {\n");
    for i in 1..=180 {
        code.push_str(&format!("    let var_{i} = {i};\n"));
    }
    code.push_str("}\n");

    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(&code, None).unwrap();

    // Change at line 90 (middle of the 182-line function)
    let (start, end, sig) =
        find_enclosing_definition(SupportedLanguage::Rust, &tree.root_node(), 90, 90, &code);
    assert_eq!(sig.as_deref().unwrap(), "fn giant_function() {");
    // Must clamp to 5 lines around line 90 rather than expanding to the full 182 lines or dropping to 0
    assert_eq!(start, 85);
    assert_eq!(end, 95);
}

#[test]
fn test_js_object_literal_does_not_block_function_expansion() {
    let js_code = "\
function configureServer() {
    const config = {
        host: 'localhost',
        port: 8080,
    };
    return config;
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::JavaScript.language_fn())
        .unwrap();
    let tree = parser.parse(js_code, None).unwrap();

    // Line 4: port: 8080,
    let (start, end, sig) = find_enclosing_definition(
        SupportedLanguage::JavaScript,
        &tree.root_node(),
        4,
        4,
        js_code,
    );
    assert_eq!(start, 1);
    assert_eq!(end, 7);
    assert!(sig.as_deref().unwrap().contains("function configureServer"));
}

#[test]
fn test_root_node_doc_comment_and_function_expansion() {
    let rust_code = "\
/// Main entry point for the service.
fn main() {
    println!(\"ready\");
}

fn other() {}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(rust_code, None).unwrap();

    // Editing both doc comment and function header (lines 1 to 2)
    let (start, end, sig) =
        find_enclosing_definition(SupportedLanguage::Rust, &tree.root_node(), 1, 2, rust_code);
    assert_eq!(start, 1);
    assert_eq!(end, 4);
    assert_eq!(sig.as_deref().unwrap(), "fn main() {");
}

#[test]
fn test_extract_target_path_ignores_plus_lines_in_hunk_body() {
    let chunk = "\
diff --git a/source.txt b/source.txt
index 1111111..2222222 100644
--- a/source.txt
+++ b/source.txt
@@ -1,2 +1,3 @@
 line1
+++ /dev/null
+line3
";
    let (target, old, is_deleted, is_new, _) = extract_target_path_from_diff_chunk(chunk).unwrap();
    assert_eq!(target, PathBuf::from("source.txt"));
    assert_eq!(old, Some(PathBuf::from("source.txt")));
    assert!(
        !is_deleted,
        "Hunk line '+++ /dev/null' must not mark chunk as deleted"
    );
    assert!(!is_new);
}

#[test]
fn test_signature_preview_skips_decorators_and_annotations() {
    // Python decorator
    let py_src = "\
@pytest.fixture
def database_client():
    return init_db()
";
    let mut parser_py = Parser::new();
    parser_py
        .set_language(&SupportedLanguage::Python.language_fn())
        .unwrap();
    let tree_py = parser_py.parse(py_src, None).unwrap();
    let (s_py, e_py, sig_py) = find_enclosing_definition(
        SupportedLanguage::Python,
        &tree_py.root_node(),
        3,
        3,
        py_src,
    );
    assert_eq!(s_py, 1);
    assert_eq!(e_py, 3);
    assert_eq!(sig_py.as_deref().unwrap(), "def database_client():");

    // Java annotation
    let java_src = "\
class Service {
    @Override
    public void execute() {
        doWork();
    }
}
";
    let mut parser_java = Parser::new();
    parser_java
        .set_language(&SupportedLanguage::Java.language_fn())
        .unwrap();
    let tree_java = parser_java.parse(java_src, None).unwrap();
    let (s_java, e_java, sig_java) = find_enclosing_definition(
        SupportedLanguage::Java,
        &tree_java.root_node(),
        4,
        4,
        java_src,
    );
    assert_eq!(s_java, 2);
    assert_eq!(e_java, 5);
    assert_eq!(
        sig_java.as_deref().unwrap(),
        "class Service > public void execute() {"
    );
}

#[test]
fn test_unsupported_language_rename_preserves_rename_header() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial_css = (0..20)
        .map(|i| format!(".class_{i} {{ color: red; }}\n"))
        .collect::<String>();
    let old_file = repo.join("style_old.css");
    std::fs::write(&old_file, &initial_css).unwrap();

    Command::new("git")
        .current_dir(repo)
        .args(["add", "style_old.css"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Git mv rename
    Command::new("git")
        .current_dir(repo)
        .args(["mv", "style_old.css", "style_new.css"])
        .status()
        .unwrap();

    // Modify 1 line in the renamed file
    let modified_css = (0..20)
        .map(|i| {
            if i == 5 {
                format!(".class_{i} {{ color: blue; }}\n")
            } else {
                format!(".class_{i} {{ color: red; }}\n")
            }
        })
        .collect::<String>();
    let new_file = repo.join("style_new.css");
    std::fs::write(&new_file, &modified_css).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "style_new.css"])
        .status()
        .unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, true, &mut warnings).unwrap();

    assert!(
        patch.contains("rename from style_old.css"),
        "Expected 'rename from style_old.css' in staged patch, got:\n{patch}"
    );
    assert!(
        patch.contains("rename to style_new.css"),
        "Expected 'rename to style_new.css' in staged patch, got:\n{patch}"
    );
}

#[test]
fn test_git_config_noprefix_and_mnemonic_prefix_supported() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    Command::new("git")
        .current_dir(repo)
        .args(["config", "diff.noprefix", "true"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["config", "diff.mnemonicPrefix", "true"])
        .status()
        .unwrap();

    let initial = "fn compute(x: i32) -> i32 {\n    let val = x;\n    val * 2\n}\n";
    let file_path = repo.join("calc.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "calc.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let modified = "fn compute(x: i32) -> i32 {\n    let val = x;\n    val * 3\n}\n";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(patch.contains("fn compute(x: i32) -> i32 {"));
    assert!(patch.contains("+    val * 3"));
    assert!(warnings.is_empty(), "Unexpected warnings: {:?}", warnings);
}

#[test]
fn test_large_file_size_limit_falls_back_to_git_diff_w_with_warning() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Create a > 2 MiB Rust file (well exceeds the ~64 KiB OS pipe buffer to verify deadlock prevention)
    let mut big = String::from("fn large_function() {\n");
    while big.len() <= 2 * 1024 * 1024 + 100 {
        big.push_str("    let x = 1;\n");
    }
    big.push_str("}\n");
    let file_path = repo.join("big.rs");
    std::fs::write(&file_path, &big).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "big.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let modified = big.replace("let x = 1;", "let x = 2;");
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(patch.contains("+    let x = 2;"));
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("exceeds tree-sitter limit")),
        "Expected size limit warning, got: {:?}",
        warnings
    );

    // Stage the large file and verify staged fallback & warning via cat-file
    Command::new("git")
        .current_dir(repo)
        .args(["add", "big.rs"])
        .status()
        .unwrap();
    let mut staged_warnings = Vec::new();
    let staged_patch = generate_expanded_diff(repo, true, &mut staged_warnings).unwrap();
    assert!(staged_patch.contains("+    let x = 2;"));
    assert!(
        staged_warnings
            .iter()
            .any(|w| w.contains("exceeds tree-sitter limit")),
        "Expected size limit warning for staged, got: {:?}",
        staged_warnings
    );
}

#[test]
#[cfg(not(windows))]
fn test_numeric_colon_filename_staged_show_and_unquoting() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let file_path = repo.join("1:calc.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "1:calc.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let modified = "fn add(a: i32, b: i32) -> i32 {\n    a * 2 + b\n}\n";
    std::fs::write(&file_path, modified).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "1:calc.rs"])
        .status()
        .unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, true, &mut warnings).unwrap();
    assert!(patch.contains("fn add(a: i32, b: i32) -> i32 {"));
    assert!(patch.contains("+    a * 2 + b"));
}

#[test]
fn test_js_nested_arrow_func_first_line_edit_does_not_escape_to_outer_func() {
    let js_src = "\
function outerComponent() {
    const helper = (val) => {
        let a = val;
        return a * 2;
    };
    return helper(10);
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::JavaScript.language_fn())
        .unwrap();
    let tree = parser.parse(js_src, None).unwrap();

    // Edit at line 2: `const helper = (val) => {`
    let (s, e, sig) = find_enclosing_definition(
        SupportedLanguage::JavaScript,
        &tree.root_node(),
        2,
        2,
        js_src,
    );
    // Must match line 2..5 (const helper), NOT line 1..7 (outerComponent)
    assert_eq!(s, 2, "Expected start at line 2, got {s}");
    assert_eq!(e, 5, "Expected end at line 5, got {e}");
    assert_eq!(sig.as_deref().unwrap(), "const helper = (val) => {");
}

#[test]
fn test_signature_preview_skips_python_comments_and_multiline_rust_attributes() {
    let py_lines = vec![
        "# This is a doc/module comment",
        "# Another comment line",
        "def process_data(items):",
        "    return len(items)",
    ];
    let sig_py = extract_signature_from_row(0, &py_lines);
    assert_eq!(sig_py.as_deref().unwrap(), "def process_data(items):");

    let rust_lines = vec![
        "#[derive(",
        "    Debug,",
        "    Clone,",
        "    PartialEq,",
        ")]",
        "pub fn run_task() {",
        "}",
    ];
    let sig_rust = extract_signature_from_row(0, &rust_lines);
    assert_eq!(sig_rust.as_deref().unwrap(), "pub fn run_task() {");

    let single_line_rust = vec!["#[inline] pub fn run_task() {", "    do_work();", "}"];
    let sig_single = extract_signature_from_row(0, &single_line_rust);
    assert_eq!(sig_single.as_deref().unwrap(), "pub fn run_task() {");

    let multiple_attrs_rust = vec![
        "#[inline] #[must_use] pub fn run_task2() {",
        "    do_work();",
        "}",
    ];
    let sig_multi = extract_signature_from_row(0, &multiple_attrs_rust);
    assert_eq!(sig_multi.as_deref().unwrap(), "pub fn run_task2() {");
}

#[test]
fn test_signature_preview_block_comments_and_multiline_decorators() {
    // Single-line block comment before signature
    let lines1 = vec!["/* inline comment */ pub fn foo() {", "    1 + 1", "}"];
    assert_eq!(
        extract_signature_from_row(0, &lines1).as_deref().unwrap(),
        "pub fn foo() {"
    );

    // Multi-line block comment ending on same line as signature
    let lines2 = vec![
        "/* block start",
        " * middle line",
        " */ pub fn bar() {",
        "    2 + 2",
        "}",
    ];
    assert_eq!(
        extract_signature_from_row(0, &lines2).as_deref().unwrap(),
        "pub fn bar() {"
    );

    // Multi-line Python decorator
    let py_lines = vec![
        "@route(",
        "    \"/api/v1/test\",",
        "    methods=[\"GET\"],",
        ")",
        "def handle_request():",
        "    pass",
    ];
    assert_eq!(
        extract_signature_from_row(0, &py_lines).as_deref().unwrap(),
        "def handle_request():"
    );
}

#[test]
fn test_special_git_mode_line_sha_not_false_positive() {
    // Mode 100644 with 120000 or 160000 in object SHA must NOT be flagged as special mode
    assert!(!is_special_git_mode_line(
        "index 8a120000f..9b3c4d5e 100644"
    ));
    assert!(!is_special_git_mode_line(
        "index 8a160000f..9b3c4d5e 100644"
    ));
    assert!(!is_special_git_mode_line("new file mode 100644"));

    // True symlink (120000) or submodule (160000) mode must be detected
    assert!(is_special_git_mode_line("index 8a120000f..9b3c4d5e 120000"));
    assert!(is_special_git_mode_line("index 8a160000f..9b3c4d5e 160000"));
    assert!(is_special_git_mode_line("new file mode 120000"));
    assert!(is_special_git_mode_line("new file mode 160000"));
    assert!(is_special_git_mode_line("old mode 120000"));
    assert!(is_special_git_mode_line("new mode 160000"));
}

#[test]
fn test_h_header_maps_to_cpp() {
    assert_eq!(
        SupportedLanguage::from_path(Path::new("header.h")),
        Some(SupportedLanguage::Cpp)
    );
    assert_eq!(
        SupportedLanguage::from_path(Path::new("file.c")),
        Some(SupportedLanguage::C)
    );
    assert_eq!(
        SupportedLanguage::from_path(Path::new("file.cpp")),
        Some(SupportedLanguage::Cpp)
    );
}

#[test]
fn test_extract_target_path_with_spaces_quotes_and_trailing_tab() {
    // Space in filename with trailing tab
    let chunk_space = "\
diff --git a/foo bar.rs b/foo bar.rs
index 1111111..2222222 100644
--- a/foo bar.rs\t
+++ b/foo bar.rs\t
@@ -1 +1 @@
-fn a() {}
+fn a() { 1; }
";
    let (target, old, is_deleted, is_new, _) =
        extract_target_path_from_diff_chunk(chunk_space).unwrap();
    assert_eq!(target, PathBuf::from("foo bar.rs"));
    assert_eq!(old, Some(PathBuf::from("foo bar.rs")));
    assert!(!is_deleted);
    assert!(!is_new);

    // Quotes in filename
    let chunk_quotes = "\
diff --git \"a/foo\\\"bar.rs\" \"b/foo\\\"bar.rs\"
index 1111111..2222222 100644
--- \"a/foo\\\"bar.rs\"
+++ \"b/foo\\\"bar.rs\"
@@ -1 +1 @@
-fn a() {}
+fn a() { 1; }
";
    let (target, old, _, _, _) = extract_target_path_from_diff_chunk(chunk_quotes).unwrap();
    assert_eq!(target, PathBuf::from("foo\"bar.rs"));
    assert_eq!(old, Some(PathBuf::from("foo\"bar.rs")));

    // Space and quote in filename with trailing tab
    let chunk_space_quote = "\
diff --git \"a/foo \\\"bar\\\".rs\" \"b/foo \\\"bar\\\".rs\"
index 1111111..2222222 100644
--- \"a/foo \\\"bar\\\".rs\"\t
+++ \"b/foo \\\"bar\\\".rs\"\t
@@ -1 +1 @@
-fn a() {}
+fn a() { 1; }
";
    let (target, old, _, _, _) = extract_target_path_from_diff_chunk(chunk_space_quote).unwrap();
    assert_eq!(target, PathBuf::from("foo \"bar\".rs"));
    assert_eq!(old, Some(PathBuf::from("foo \"bar\".rs")));

    // Fallback diff --git with spaces (e.g. mode change)
    let chunk_mode = "\
diff --git a/foo bar.rs b/foo bar.rs
old mode 100644
new mode 100755
";
    let (target, old, _, _, _) = extract_target_path_from_diff_chunk(chunk_mode).unwrap();
    assert_eq!(target, PathBuf::from("foo bar.rs"));
    assert_eq!(old, Some(PathBuf::from("foo bar.rs")));

    // Fallback diff --git with quotes
    let chunk_mode_quotes = "\
diff --git \"a/foo\\\"bar.rs\" \"b/foo\\\"bar.rs\"
old mode 100644
new mode 100755
";
    let (target, old, _, _, _) = extract_target_path_from_diff_chunk(chunk_mode_quotes).unwrap();
    assert_eq!(target, PathBuf::from("foo\"bar.rs"));
    assert_eq!(old, Some(PathBuf::from("foo\"bar.rs")));
}

#[test]
fn test_crlf_and_eol_conversion_preserves_newline() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial = "fn win_fn() -> i32 {\r\n    let a = 1;\r\n    let b = 2;\r\n    a + b\r\n}\r\n";
    let file_path = repo.join("win.rs");
    std::fs::write(&file_path, initial.as_bytes()).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "win.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Modify 1 line inside CRLF file
    let modified =
        "fn win_fn() -> i32 {\r\n    let a = 1;\r\n    let b = 99;\r\n    a + b\r\n}\r\n";
    std::fs::write(&file_path, modified.as_bytes()).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(patch.contains("\r\n"));

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "win.rs"])
        .status()
        .unwrap();
    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_mixed_eol_and_crlf_no_trailing_newline_git_apply() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Mixed EOL file with no trailing newline
    let initial = b"fn mixed_fn() {\r\n    let a = 1;\n    let b = 2;\r\n    a + b}";
    let file_path = repo.join("mixed.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "mixed.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let modified = b"fn mixed_fn() {\r\n    let a = 99;\n    let b = 2;\r\n    a + b}";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(patch.contains("\\ No newline at end of file\n"));

    // Verify git apply --check passes cleanly without EOL corruption
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "mixed.rs"])
        .status()
        .unwrap();
    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git apply failed on mixed EOL: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_crlf_to_lf_conversion_git_apply() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let crlf = "fn convert() -> i32 {\r\n    let x = 1;\r\n    let y = 2;\r\n    x + y\r\n}\r\n";
    let file_path = repo.join("conv.rs");
    std::fs::write(&file_path, crlf.as_bytes()).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "conv.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Convert entire file from CRLF to LF
    let lf = crlf.replace("\r\n", "\n");
    std::fs::write(&file_path, lf.as_bytes()).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "conv.rs"])
        .status()
        .unwrap();
    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git apply failed on CRLF->LF conversion: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_commit_expanded_with_tree_sitter_and_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial_rust = "\
fn helper() -> i32 {
    1
}

fn compute(a: i32, b: i32) -> i32 {
    let x = a;
    let y = b;
    x + y
}

fn other() -> i32 {
    3
}
";
    let initial_css = "\
.container {
    margin: 0;
    padding: 10px;
}
";
    std::fs::write(repo.join("math.rs"), initial_rust).unwrap();
    std::fs::write(repo.join("style.css"), initial_css).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "math.rs", "style.css"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Second commit: modify math.rs and style.css
    let mod_rust = initial_rust.replace("x + y", "x * 2 + y");
    let mod_css = initial_css.replace("padding: 10px;", "padding: 20px;");
    std::fs::write(repo.join("math.rs"), mod_rust).unwrap();
    std::fs::write(repo.join("style.css"), mod_css).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "math.rs", "style.css"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "feature update commit"])
        .status()
        .unwrap();

    let sha_out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let full_sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

    let mut warnings = Vec::new();
    let patch = generate_expanded_commit_diff(repo, &full_sha, &mut warnings).unwrap();

    // 1. Commit header preserved
    assert!(patch.contains(&format!("commit {full_sha}")));
    assert!(patch.contains("feature update commit"));

    // 2. Supported Rust file expanded with Tree-sitter enclosing function
    assert!(patch.contains("fn compute(a: i32, b: i32) -> i32 {"));
    assert!(patch.contains("+    x * 2 + y"));

    // 3. Unsupported CSS file fallback via git show -W
    assert!(patch.contains(".container {"));
    assert!(patch.contains("+    padding: 20px;"));
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("unsupported language for 'style.css'")),
        "Expected unsupported language warning, got: {:?}",
        warnings
    );

    // 4. Temporary patch file creation via git_commit_expanded_file
    let mut file_warnings = Vec::new();
    let patch_path = git_commit_expanded_file(repo, &full_sha, &mut file_warnings).unwrap();
    let short = &full_sha[..12];
    assert_eq!(
        patch_path.file_name().unwrap().to_str().unwrap(),
        &format!("git-{short}+.patch")
    );
    assert!(patch_path.exists());
    let _ = std::fs::remove_file(&patch_path);
    if let Some(parent) = patch_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
#[cfg(unix)]
fn test_symlink_not_expanded_with_treesitter() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    std::fs::write(repo.join("target1.rs"), "fn target1() -> i32 { 1 }\n").unwrap();
    std::fs::write(repo.join("target2.rs"), "fn target2() -> i32 { 2 }\n").unwrap();
    symlink("target1.rs", repo.join("link.rs")).unwrap();

    Command::new("git")
        .current_dir(repo)
        .args(["add", "target1.rs", "target2.rs", "link.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Change symlink to target2.rs
    std::fs::remove_file(repo.join("link.rs")).unwrap();
    symlink("target2.rs", repo.join("link.rs")).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    // Symlink diff must NOT include expanded body of target.rs
    assert!(!patch.contains("fn target2() -> i32"));
    assert!(!patch.contains("fn target1() -> i32"));

    // Restore link.rs to committed state before testing git apply
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "link.rs"])
        .status()
        .unwrap();

    // Verify git apply --check passes cleanly
    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git apply failed on symlink diff: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_bracket_path_literal_pathspecs() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let bracket_dir = repo.join("routes").join("[id]");
    std::fs::create_dir_all(&bracket_dir).unwrap();
    let file_path = bracket_dir.join("+page.svelte");
    std::fs::write(&file_path, "<h1>Initial [ID]</h1>\n").unwrap();

    Command::new("git")
        .current_dir(repo)
        .args(["add", "."])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    std::fs::write(&file_path, "<h1>Updated [ID] Page</h1>\n").unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    assert!(patch.contains("routes/[id]/+page.svelte"));
    assert!(patch.contains("+<h1>Updated [ID] Page</h1>"));
}

#[test]
fn test_commit_message_containing_diff_git() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    std::fs::write(repo.join("foo.txt"), "hello\n").unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "foo.txt"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "feat: discuss diff --git in message"])
        .status()
        .unwrap();

    let sha_out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

    let mut warnings = Vec::new();
    let patch = generate_expanded_commit_diff(repo, &sha, &mut warnings).unwrap();

    // Commit header must be fully preserved with author and message
    assert!(patch.contains("feat: discuss diff --git in message"));
    assert!(patch.contains(&format!("commit {sha}")));
}

#[test]
fn test_signature_extraction_skips_multiline_block_comments_and_inner_attributes() {
    let source = "\
/*
  This is a multi-line block comment
  describing the file and license.
*/
#![allow(unused)]
#![feature(test)]

fn target_function(x: i32) -> i32 {
    x * 2
}
";
    let lines: Vec<&str> = source.lines().collect();
    let sig = extract_signature_from_row(0, &lines);
    assert_eq!(sig.as_deref(), Some("fn target_function(x: i32) -> i32 {"));
}

#[test]
fn test_js_pair_and_assignment_primary_definition() {
    let source = "\
const service = {
    handler: () => {
        const x = 1;
        return x;
    }
};

obj.compute = function() {
    return 42;
};
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::JavaScript.language_fn())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    // Line 2 is `handler: () => {`
    let (start, end, sig) =
        find_enclosing_definition(SupportedLanguage::JavaScript, &root, 2, 2, source);
    assert_eq!(start, 2);
    assert_eq!(end, 5);
    assert_eq!(sig.as_deref(), Some("handler: () => {"));

    // Line 8 is `obj.compute = function() {`
    let (start8, end8, sig8) =
        find_enclosing_definition(SupportedLanguage::JavaScript, &root, 8, 8, source);
    assert_eq!(start8, 8);
    assert_eq!(end8, 10);
    assert_eq!(sig8.as_deref(), Some("obj.compute = function() {"));
}

#[test]
fn test_lockfile_and_generated_omission() {
    assert!(is_lockfile_or_generated(Path::new("Cargo.lock")));
    assert!(is_lockfile_or_generated(Path::new("sub/package-lock.json")));
    assert!(is_lockfile_or_generated(Path::new("yarn.lock")));
    assert!(is_lockfile_or_generated(Path::new("vendor/pnpm-lock.yaml")));
    assert!(is_lockfile_or_generated(Path::new("dist/app.min.js")));
    assert!(is_lockfile_or_generated(Path::new("dist/style.min.css")));
    assert!(is_lockfile_or_generated(Path::new("uv.lock")));
    assert!(is_lockfile_or_generated(Path::new("pdm.lock")));
    assert!(is_lockfile_or_generated(Path::new("package.resolved")));
    assert!(is_lockfile_or_generated(Path::new("Podfile.lock")));
    assert!(is_lockfile_or_generated(Path::new("pubspec.lock")));
    assert!(is_lockfile_or_generated(Path::new("mix.lock")));
    assert!(is_lockfile_or_generated(Path::new("gradle.lockfile")));
    assert!(is_lockfile_or_generated(Path::new("packages.lock.json")));
    assert!(is_lockfile_or_generated(Path::new("api.pb.go")));
    assert!(is_lockfile_or_generated(Path::new("gen_pb2.py")));
    assert!(is_lockfile_or_generated(Path::new("state.g.dart")));
    assert!(is_lockfile_or_generated(Path::new("Form1.designer.cs")));
    assert!(!is_lockfile_or_generated(Path::new("snapshot.snap")));
    assert!(!is_lockfile_or_generated(Path::new("Cargo.toml")));
    assert!(!is_lockfile_or_generated(Path::new("src/main.rs")));

    let dummy_chunk = "\
diff --git a/Cargo.lock b/Cargo.lock
index 1111111..2222222 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,3 +10,4 @@
-old_dep = 1
+new_dep = 2
+another_dep = 3
";
    let summarized = summarize_omitted_chunk(dummy_chunk, Path::new("Cargo.lock"));
    assert!(summarized.contains("diff --git a/Cargo.lock b/Cargo.lock"));
    assert!(summarized.contains("[omitted: Cargo.lock (+2/-1 lines)]"));
    assert!(!summarized.contains("old_dep"));
}

#[test]
fn test_multiline_signature_compaction_and_whitespace_collapse() {
    let code = "\
pub fn resolve_git_mention(
    workspace: &Path,
    spec: &str,
    git_cache: &mut HashMap<GitPatchTarget, PathBuf>,
) -> Result<PathBuf> {
    let x = 1;
    Ok(x)
}
";
    let lines: Vec<&str> = code.lines().collect();
    let sig = extract_signature_from_row(0, &lines);
    assert!(sig.is_some());
    let s = sig.unwrap();
    // Newlines should be flattened into single spaces and '{' preserved
    assert!(!s.contains('\n'));
    assert!(s.starts_with("pub fn resolve_git_mention("));
    assert!(s.contains("-> Result<PathBuf> {"));
}

#[test]
fn test_parent_scope_prefix_rust_impl() {
    let code = "\
impl MentionPopup {
    pub(super) fn new(items: Vec<String>) -> Self {
        Self { items }
    }
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();
    let (start, end, sig) =
        find_enclosing_definition(SupportedLanguage::Rust, &tree.root_node(), 2, 3, code);
    assert_eq!(start, 2);
    assert_eq!(end, 4);
    assert_eq!(
        sig.as_deref().unwrap(),
        "impl MentionPopup > pub(super) fn new(items: Vec<String>) -> Self {"
    );
}

#[test]
fn test_clamped_state_header_and_low_churn_clamping() {
    // 1. Test function (>40 lines) clamping
    let mut test_code = String::from("#[test]\nfn test_big_case() {\n");
    for i in 1..=50 {
        test_code.push_str(&format!("    assert_eq!({i}, {i});\n"));
    }
    test_code.push_str("}\n");

    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(&test_code, None).unwrap();

    // Change at line 25 of 52-line test function
    let (s, e, sig, is_clamped) = find_enclosing_definition_full(
        SupportedLanguage::Rust,
        &tree.root_node(),
        25,
        25,
        &test_code,
        Some(Path::new("tests/my_test.rs")),
        0,
    );
    assert!(is_clamped);
    assert_eq!(s, 20); // 25 - 5
    assert_eq!(e, 30); // 25 + 5
    assert!(sig.as_deref().unwrap().starts_with("fn test_big_case() {"));

    // 2. Low churn large production function (95 lines, 1 line changed)
    let mut prod_code = String::from("fn prepare_internal() {\n");
    for i in 1..=93 {
        prod_code.push_str(&format!("    let step_{i} = {i};\n"));
    }
    prod_code.push_str("}\n");
    let tree_prod = parser.parse(&prod_code, None).unwrap();
    let (s_prod, e_prod, sig_prod, is_clamped_prod) = find_enclosing_definition_full(
        SupportedLanguage::Rust,
        &tree_prod.root_node(),
        50,
        50,
        &prod_code,
        Some(Path::new("src/input.rs")),
        0,
    );
    assert!(is_clamped_prod);
    assert_eq!(s_prod, 45); // 50 - 5
    assert_eq!(e_prod, 55); // 50 + 5
    assert_eq!(sig_prod.as_deref().unwrap(), "fn prepare_internal() {");
}

#[test]
fn test_compact_signature_preview_preserves_return_types_and_visibilities() {
    // Return type tuple: -> (usize, usize, Option<String>) must be preserved!
    let sig = "pub fn find_enclosing_definition(lang: SupportedLanguage, root: &tree_sitter::Node, start_line: usize, end_line: usize, source: &str) -> (usize, usize, Option<String>) {";
    let compacted = compact_signature_preview(sig, 120);
    assert!(compacted.contains("-> (usize, usize, Option<String>)"));
    assert!(compacted.starts_with("pub fn find_enclosing_definition("));
    assert!(compacted.ends_with('{'));

    // Visibility modifier with paren: pub(crate) fn foo(...)
    let sig_crate = "pub(crate) fn very_long_function_name(param1: VeryLongTypeName1, param2: VeryLongTypeName2, param3: VeryLongTypeName3) -> Result<(), Error> {";
    let compacted_crate = compact_signature_preview(sig_crate, 100);
    assert!(compacted_crate.starts_with("pub(crate) fn very_long_function_name("));
    assert!(compacted_crate.contains("-> Result<(), Error> {"));
}

#[test]
fn test_signature_extraction_with_destructuring_and_url_strings() {
    let code = "\
function Button({
    label,
    onClick,
}: {
    label: string;
    onClick: () => void;
}) {
    return null;
}
";
    let lines: Vec<&str> = code.lines().collect();
    let sig = extract_signature_from_row(0, &lines);
    assert!(sig.is_some());
    let s = sig.unwrap();
    assert!(s.starts_with("function Button({"));
    assert!(s.contains("onClick"));
    assert!(s.ends_with('{'));

    // String literal containing // should not be stripped as a comment
    let code_url = "\
pub fn fetch_data(
    endpoint: &str = \"https://example.com/api/v1\",
    timeout: u64,
) -> Result<String, Error> {
";
    let lines_url: Vec<&str> = code_url.lines().collect();
    let sig_url = extract_signature_from_row(0, &lines_url);
    assert!(sig_url.is_some());
    let s_url = sig_url.unwrap();
    assert!(s_url.contains("https://example.com/api/v1"));
    assert!(s_url.contains("-> Result<String, Error> {"));
}

#[test]
fn test_is_test_scope_avoids_false_positives() {
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree_dummy = parser.parse("fn foo() {}", None).unwrap();

    assert!(!is_test_scope(
        SupportedLanguage::Rust,
        &tree_dummy.root_node(),
        Some(Path::new("src/contest.rs")),
        &[]
    ));
    assert!(!is_test_scope(
        SupportedLanguage::Rust,
        &tree_dummy.root_node(),
        Some(Path::new("src/latest.rs")),
        &[]
    ));
    assert!(!is_test_scope(
        SupportedLanguage::Rust,
        &tree_dummy.root_node(),
        Some(Path::new("src/shortest.rs")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::Rust,
        &tree_dummy.root_node(),
        Some(Path::new("src/test.rs")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::Rust,
        &tree_dummy.root_node(),
        Some(Path::new("tests/sub/foo.rs")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::Tsx,
        &tree_dummy.root_node(),
        Some(Path::new("src/Button.test.tsx")),
        &[]
    ));

    // Java, Kotlin, C# PascalCase vs false positives
    assert!(!is_test_scope(
        SupportedLanguage::Java,
        &tree_dummy.root_node(),
        Some(Path::new("src/Contest.java")),
        &[]
    ));
    assert!(!is_test_scope(
        SupportedLanguage::Java,
        &tree_dummy.root_node(),
        Some(Path::new("src/Latest.kt")),
        &[]
    ));
    assert!(!is_test_scope(
        SupportedLanguage::CSharp,
        &tree_dummy.root_node(),
        Some(Path::new("src/Shortest.cs")),
        &[]
    ));
    assert!(!is_test_scope(
        SupportedLanguage::Java,
        &tree_dummy.root_node(),
        Some(Path::new("src/Protests.java")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::Java,
        &tree_dummy.root_node(),
        Some(Path::new("src/OrderTest.java")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::CSharp,
        &tree_dummy.root_node(),
        Some(Path::new("src/OrderTests.cs")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::Java,
        &tree_dummy.root_node(),
        Some(Path::new("src/OrderSpec.kt")),
        &[]
    ));

    // Helper below a #[test] function should NOT be classified as test
    let code = "\
#[test]
fn test_something() {
    assert!(true);
}

fn regular_helper() {
    let x = 1;
}
";
    let lines: Vec<&str> = code.lines().collect();
    let tree = parser.parse(code, None).unwrap();
    let helper_node = tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 5, column: 0 },
            tree_sitter::Point { row: 5, column: 10 },
        )
        .unwrap();
    assert!(!is_test_scope(
        SupportedLanguage::Rust,
        &helper_node,
        Some(Path::new("src/lib.rs")),
        &lines
    ));

    // Helper below a comment containing #[test] should NOT be classified as test
    let comment_code = "\
// not #[test]
fn helper_with_comment() {
    let x = 1;
}
";
    let comment_lines: Vec<&str> = comment_code.lines().collect();
    let comment_tree = parser.parse(comment_code, None).unwrap();
    let comment_fn_node = comment_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 1, column: 0 },
            tree_sitter::Point { row: 1, column: 15 },
        )
        .unwrap();
    assert!(!is_test_scope(
        SupportedLanguage::Rust,
        &comment_fn_node,
        Some(Path::new("src/lib.rs")),
        &comment_lines
    ));

    // Multi-line #[tokio::test(...)] attribute
    let tokio_code = "\
#[tokio::test(
    flavor = \"multi_thread\"
)]
async fn my_async_test() {
    assert!(true);
}
";
    let tokio_lines: Vec<&str> = tokio_code.lines().collect();
    let tokio_tree = parser.parse(tokio_code, None).unwrap();
    let tokio_fn_node = tokio_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 3, column: 0 },
            tree_sitter::Point { row: 3, column: 20 },
        )
        .unwrap();
    assert!(is_test_scope(
        SupportedLanguage::Rust,
        &tokio_fn_node,
        Some(Path::new("src/lib.rs")),
        &tokio_lines
    ));

    // Helper inside #[cfg(test)] mod tests WITHOUT test_ prefix
    let mod_code = "\
#[cfg(test)]
mod tests {
    fn helper_without_prefix() {
        let y = 2;
    }
}
";
    let mod_lines: Vec<&str> = mod_code.lines().collect();
    let mod_tree = parser.parse(mod_code, None).unwrap();
    let mod_fn_node = mod_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 2, column: 4 },
            tree_sitter::Point { row: 2, column: 28 },
        )
        .unwrap();
    assert!(is_test_scope(
        SupportedLanguage::Rust,
        &mod_fn_node,
        Some(Path::new("src/lib.rs")),
        &mod_lines
    ));

    // Java test method with @Test and helper below
    let mut java_parser = Parser::new();
    java_parser
        .set_language(&SupportedLanguage::Java.language_fn())
        .unwrap();
    let java_code = "\
class MyService {
    @Test
    public void testMethod() {
        helper();
    }

    public void helper() {
    }
}
";
    let java_lines: Vec<&str> = java_code.lines().collect();
    let java_tree = java_parser.parse(java_code, None).unwrap();
    let test_method_node = java_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 2, column: 4 },
            tree_sitter::Point { row: 2, column: 20 },
        )
        .unwrap();
    let helper_method_node = java_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 6, column: 4 },
            tree_sitter::Point { row: 6, column: 20 },
        )
        .unwrap();
    assert!(is_test_scope(
        SupportedLanguage::Java,
        &test_method_node,
        Some(Path::new("src/MyService.java")),
        &java_lines
    ));
    assert!(!is_test_scope(
        SupportedLanguage::Java,
        &helper_method_node,
        Some(Path::new("src/MyService.java")),
        &java_lines
    ));
}

#[test]
fn test_parent_scope_prefix_capped_at_45_chars() {
    let code = "\
impl<T: VeryLongTraitBoundOne + VeryLongTraitBoundTwo> VeryLongTypeNameForContainer<T> {
    pub fn do_something() -> Result<(), Error> {
        Ok(())
    }
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();
    let (_, _, sig) =
        find_enclosing_definition(SupportedLanguage::Rust, &tree.root_node(), 2, 2, code);
    let s = sig.expect("expected signature");
    assert!(s.contains("... > pub fn do_something()"));
    assert!(s.contains("-> Result<(), Error> {"));
}

#[test]
fn test_lockfile_binary_omitted_and_churn_count() {
    // Binary lockfile chunk
    let binary_chunk = "\
diff --git a/bun.lockb b/bun.lockb
index 1234567..89abcdef 100644
Binary files a/bun.lockb and b/bun.lockb differ
";
    let summarized = summarize_omitted_chunk(binary_chunk, Path::new("bun.lockb"));
    assert!(summarized.contains("[omitted: bun.lockb (binary)]"));

    // Markdown --- deletion churn count
    let md_chunk = "\
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -10,3 +10,2 @@
-line1
----
+line2
";
    let (added, deleted) = count_diff_churn(md_chunk);
    assert_eq!(added, 1);
    assert_eq!(deleted, 2, "Expected 2 deletions including '---'");
}

#[test]
fn test_pure_deletion_not_low_churn_in_large_function() {
    let mut code = String::from("fn large_function() {\n");
    for i in 1..=65 {
        code.push_str(&format!("    let var_{i} = {i};\n"));
    }
    code.push_str("}\n");

    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(&code, None).unwrap();

    // 20 lines deleted at line 20
    let (s, e, _sig, is_clamped) = find_enclosing_definition_full(
        SupportedLanguage::Rust,
        &tree.root_node(),
        20,
        20,
        &code,
        Some(Path::new("src/compute.rs")),
        20, // 20 lines deleted!
    );
    // Function is 67 lines <= MAX_EXPANSION_LINES (150), and NOT low-churn because 20 lines deleted.
    // So it should NOT be clamped!
    assert!(
        !is_clamped,
        "Pure deletion of 20 lines should not be clamped"
    );
    assert_eq!(s, 1);
    assert_eq!(e, 67);
}

#[test]
fn test_diff_preamble_and_git_apply_compatibility() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let run_git = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };

    std::fs::write(repo.join("hello.rs"), "fn hello() {}\n").unwrap();
    run_git(&["add", "hello.rs"]);
    run_git(&["commit", "-m", "init"]);

    // Modify hello.rs unstaged
    std::fs::write(
        repo.join("hello.rs"),
        "fn hello() {\n    println!(\"world\");\n}\n",
    )
    .unwrap();

    let mut warnings = Vec::new();
    let patch_text = generate_expanded_diff(repo, false, &mut warnings).unwrap();

    // Preamble check
    assert!(patch_text.starts_with("Target: unstaged (index -> worktree)\n\n"));
    assert!(patch_text.contains("hello.rs"));
    assert!(patch_text.contains("diff --git a/hello.rs b/hello.rs"));

    // Must still be compatible with git apply
    let patch_file = repo.join("test.patch");
    std::fs::write(&patch_file, &patch_text).unwrap();

    // Revert worktree before testing apply
    run_git(&["checkout", "--", "hello.rs"]);
    let apply_out = Command::new("git")
        .current_dir(repo)
        .args(["apply", "--check"])
        .arg(&patch_file)
        .output()
        .unwrap();
    assert!(
        apply_out.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&apply_out.stderr)
    );
}

#[test]
fn test_signature_extraction_with_rust_lifetimes_and_ast() {
    let code = "\
fn generate_diff_preamble(repo_root: &Path, source: GitDiffSource<'_>) -> String {
    let target_desc = match source {
        GitDiffSource::Staged => \"staged\",
        _ => \"other\",
    };
    target_desc.to_string()
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let fn_node = tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 0, column: 0 },
            tree_sitter::Point { row: 0, column: 20 },
        )
        .unwrap();
    let sig = extract_signature(SupportedLanguage::Rust, &fn_node, &lines, code);
    assert_eq!(
        sig.as_deref(),
        Some("fn generate_diff_preamble(repo_root: &Path, source: GitDiffSource<'_>) -> String {")
    );

    // Also test row-based fallback with Rust lifetime and char literal
    let row_sig = extract_signature_from_row(0, &lines);
    assert_eq!(
        row_sig.as_deref(),
        Some("fn generate_diff_preamble(repo_root: &Path, source: GitDiffSource<'_>) -> String {")
    );
}

#[test]
fn test_java_signature_skips_override_annotation() {
    let code = "\
class Service {
    @Override
    public void executeTask() {
        doWork();
    }
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Java.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let method_node = tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 2, column: 4 },
            tree_sitter::Point { row: 2, column: 20 },
        )
        .unwrap();
    let sig = extract_signature(SupportedLanguage::Java, &method_node, &lines, code);
    assert_eq!(sig.as_deref(), Some("public void executeTask() {"));
}

#[test]
fn test_python_signature_ast_extraction() {
    let code = "\
def calculate_metrics(data: list[int]) -> int:
    total = sum(data)
    return total
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Python.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let fn_node = tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 0, column: 0 },
            tree_sitter::Point { row: 0, column: 20 },
        )
        .unwrap();
    let sig = extract_signature(SupportedLanguage::Python, &fn_node, &lines, code);
    assert_eq!(
        sig.as_deref(),
        Some("def calculate_metrics(data: list[int]) -> int:")
    );
}

#[test]
fn test_snap_files_not_omitted_as_lockfiles() {
    assert!(!is_lockfile_or_generated(Path::new("test.snap")));
    assert!(!is_lockfile_or_generated(Path::new("tests/snapshot.snap")));
}

#[test]
fn test_attribute_only_lookahead_and_non_definition_context() {
    let code = "\
use std::collections::HashMap;

#[cfg(test)]
fn test_something() {
    assert!(true);
}
";
    let lines: Vec<&str> = code.lines().collect();
    // Line 1: `use std::collections::HashMap;` (1-indexed) is NOT attribute only
    assert!(!is_attribute_or_comment_only(
        &lines,
        1,
        1,
        SupportedLanguage::Rust
    ));

    // Line 3: `#[cfg(test)]` (1-indexed) IS attribute only
    assert!(is_attribute_or_comment_only(
        &lines,
        3,
        3,
        SupportedLanguage::Rust
    ));
}

#[test]
fn test_signature_compaction_and_generic_fn_bound() {
    // 1. split_first_param_at_depth_zero does not decrement on -> or =>
    let inner = "f: Foo<fn() -> A, B>, next: i32";
    let (first, has_more) = split_first_param_at_depth_zero(inner, true);
    assert_eq!(first, "f: Foo<fn() -> A, B>");
    assert!(has_more);

    // 2. find_parameter_list_span ignores Fn(...) inside <...> generic bounds
    let sig = "pub fn apply<F: Fn(usize) -> bool>(f: F, x: usize) -> bool {";
    let span = find_parameter_list_span(sig, true).expect("expected param span");
    assert_eq!(&sig[span.0..=span.1], "(f: F, x: usize)");

    // 3. compact_signature_preview: single parameter should not have ", ..."
    let single_param =
        "pub fn execute_operation(context: &mut ExecutionContext) -> Result<Status, Error> {";
    let compacted_single = compact_signature_preview(single_param, 60);
    assert!(
        !compacted_single.contains(", ..."),
        "Single parameter must not contain ', ...', got: {compacted_single}"
    );

    // Multiple parameters should have ", ..." when candidate1 fits (e.g. max_chars = 85)
    let multi_param = "pub fn execute_operation(ctx: &mut Context, val: i32, flag: bool) -> Result<Status, Error> {";
    let compacted_multi = compact_signature_preview(multi_param, 85);
    assert!(
        compacted_multi.contains(", ..."),
        "Multiple parameters should contain ', ...', got: {compacted_multi}"
    );
    // And when candidate1 exceeds max_chars (e.g. max_chars = 60), it falls back to (...)
    let compacted_multi_short = compact_signature_preview(multi_param, 60);
    assert_eq!(
        compacted_multi_short,
        "pub fn execute_operation(...) -> Result<Status, Error> {"
    );
}

#[test]
fn test_cfg_test_attribute_and_rejection() {
    assert!(is_cfg_test_attribute("#[cfg(test)]"));
    assert!(is_cfg_test_attribute(
        "#[cfg(all(test, feature = \"foo\"))]"
    ));
    assert!(is_cfg_test_attribute(
        "#[cfg(any(feature = \"foo\", test))]"
    ));

    // Must reject not(test), attestation, and cfg_attr
    assert!(!is_cfg_test_attribute("#[cfg(not(test))]"));
    assert!(!is_cfg_test_attribute("#[cfg(feature = \"attestation\")]"));
    assert!(!is_cfg_test_attribute("#[cfg(feature = \"contest\")]"));
    assert!(!is_cfg_test_attribute("#[cfg_attr(test, derive(Debug))]"));

    // Multi-attribute slice precision
    assert!(!is_cfg_test_attribute(
        "#[cfg(feature = \"foo\")]\n#[cfg_attr(test, derive(Debug))]"
    ));
    assert!(is_cfg_test_attribute(
        "#[cfg_attr(not(test), inline)]\n#[cfg(test)]"
    ));
    assert!(is_cfg_test_attribute("#[cfg(not(test))]\n#[cfg(test)]"));
}

#[test]
fn test_attribute_or_comment_rejections() {
    // Dereference assignment must not be treated as comment
    let lines = ["*ptr = 1;"];
    assert!(!is_attribute_or_comment_only(
        &lines,
        1,
        1,
        SupportedLanguage::Rust
    ));

    // Array literal and assignment must not be treated as C# attribute (e.g. in JS/Rust)
    let array_lines = ["[1, 2, 3]"];
    assert!(!is_attribute_or_comment_only(
        &array_lines,
        1,
        1,
        SupportedLanguage::JavaScript
    ));
    assert!(!is_attribute_or_comment_only(
        &array_lines,
        1,
        1,
        SupportedLanguage::Rust
    ));

    let assign_lines = ["[a = 1]"];
    assert!(!is_attribute_or_comment_only(
        &assign_lines,
        1,
        1,
        SupportedLanguage::Python
    ));

    // Valid comment continuation and C# attribute (including named arguments)
    let comment_cont = ["* block comment continuation"];
    assert!(is_attribute_or_comment_only(
        &comment_cont,
        1,
        1,
        SupportedLanguage::Rust
    ));

    let cs_attr = ["[HttpGet]"];
    assert!(is_attribute_or_comment_only(
        &cs_attr,
        1,
        1,
        SupportedLanguage::CSharp
    ));
    assert!(!is_attribute_or_comment_only(
        &cs_attr,
        1,
        1,
        SupportedLanguage::Rust
    ));

    let cs_fact = ["[Fact(Skip = \"reason\")]"];
    assert!(is_attribute_or_comment_only(
        &cs_fact,
        1,
        1,
        SupportedLanguage::CSharp
    ));

    let cs_testcase = ["[TestCase(1, \"foo\", Bar = 2)]"];
    assert!(is_attribute_or_comment_only(
        &cs_testcase,
        1,
        1,
        SupportedLanguage::CSharp
    ));

    // Python # without space (e.g. #TODO) must be recognized as comment
    let py_comment = ["#TODO: implement this"];
    assert!(is_attribute_or_comment_only(
        &py_comment,
        1,
        1,
        SupportedLanguage::Python
    ));
}

#[test]
fn test_scanner_escaped_quotes_and_python_single_quotes() {
    // Escaped backslash in string
    let row_lines = [
        "fn parse(pattern: &str = \"\\\\\") {",
        "    let x = 1;",
        "}",
    ];
    let sig = extract_signature_from_row(0, &row_lines);
    assert_eq!(sig.as_deref(), Some("fn parse(pattern: &str = \"\\\\\") {"));

    // Python single-quoted string with { and //
    let py_lines = [
        "def query(url: str = 'https://example.com/{id}') -> dict:",
        "    return {}",
    ];
    let py_sig = extract_signature_from_row(0, &py_lines);
    assert_eq!(
        py_sig.as_deref(),
        Some("def query(url: str = 'https://example.com/{id}') -> dict:")
    );
}

#[test]
fn test_ast_signature_inline_comment_and_nullable_java() {
    // Rust function with comment between params and return type
    let rust_code = "\
pub fn compute_sum(
    a: i32, // first parameter
    b: i32,
) -> i32 {
    a + b
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(rust_code, None).unwrap();
    let lines: Vec<&str> = rust_code.lines().collect();
    let fn_node = tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 0, column: 0 },
            tree_sitter::Point { row: 0, column: 20 },
        )
        .unwrap();
    let sig = extract_signature(SupportedLanguage::Rust, &fn_node, &lines, rust_code);
    let s = sig.expect("expected signature");
    assert!(s.starts_with("pub fn compute_sum("));
    assert!(!s.contains("//"), "Signature must not contain '//': {s}");
    assert!(s.contains("b: i32"));
    assert!(s.ends_with(") -> i32 {"));

    // Java method with @Nullable return type modifier
    let java_code = "\
class ItemService {
    public @Nullable String getItemName() {
        return null;
    }
}
";
    let mut java_parser = Parser::new();
    java_parser
        .set_language(&SupportedLanguage::Java.language_fn())
        .unwrap();
    let java_tree = java_parser.parse(java_code, None).unwrap();
    let java_lines: Vec<&str> = java_code.lines().collect();
    let method_node = java_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 1, column: 4 },
            tree_sitter::Point { row: 1, column: 25 },
        )
        .unwrap();
    let java_sig = extract_signature(
        SupportedLanguage::Java,
        &method_node,
        &java_lines,
        java_code,
    );
    let js = java_sig.expect("expected java signature");
    assert!(
        js.starts_with("public "),
        "Java signature must retain 'public', got: {js}"
    );
    assert!(js.contains("String getItemName() {"));
}

#[test]
fn test_char_literal_and_lifetime_and_scanner_multiline_fixes() {
    // 1. char_literal_len \x escape
    assert_eq!(char_literal_len(r"'\x7f'"), Some(6));
    assert_eq!(char_literal_len(r"'\x00'"), Some(6));
    assert_eq!(char_literal_len(r"'\n'"), Some(4));
    assert_eq!(char_literal_len(r"'a'"), Some(3));

    // 2. strip_line_comment_simple with char literals and # comments
    assert_eq!(
        strip_line_comment_simple("fn f(c: char = '#')", false, true),
        "fn f(c: char = '#')"
    );
    assert_eq!(
        strip_line_comment_simple("fn f(c: char = 'a') // comment", false, true),
        "fn f(c: char = 'a')"
    );
    assert_eq!(
        strip_line_comment_simple("pub fn parse(r#type: Kind)", false, true),
        "pub fn parse(r#type: Kind)"
    );
    let raw_str = "const S: &str = r#\"hello\"#;";
    assert_eq!(strip_line_comment_simple(raw_str, false, true), raw_str);
    assert_eq!(
        strip_line_comment_simple("#helper() {", false, false),
        "#helper() {"
    );
    assert_eq!(
        strip_line_comment_simple("def f(x: int): # python comment", true, false),
        "def f(x: int):"
    );
    // Python/Bash/Ruby with # without space (e.g. #TODO or def f(bar#comment)
    assert_eq!(
        strip_line_comment_simple("def foo(x: int): #TODO", true, false),
        "def foo(x: int):"
    );
    assert_eq!(
        strip_line_comment_simple("def f(bar#comment", true, false),
        "def f(bar"
    );
    assert_eq!(strip_line_comment_simple("#comment", true, false), "");
    // Non-Rust single-quoted string with spaces/symbols must NOT be treated as Rust lifetime
    assert_eq!(
        strip_line_comment_simple(
            "function render(mode: 'dark mode' | 'light mode') {",
            false,
            false
        ),
        "function render(mode: 'dark mode' | 'light mode') {"
    );

    // 3. SigScanState::scan_line does not prematurely end on ':' mid-line
    let mut scanner = SigScanState::default();
    let (ends, _) = scanner.scan_line("pub fn foo<T: Clone>(");
    assert!(
        !ends,
        "Colon inside generic parameter must not end signature on line 1"
    );
    assert_eq!(scanner.p_depth, 1);

    let mut class_scanner = SigScanState::default();
    let (c_ends, _) = class_scanner.scan_line("void MyClass::method(");
    assert!(!c_ends, "Double colon must not end signature on line 1");
    assert_eq!(class_scanner.p_depth, 1);

    let mut where_scanner = SigScanState::default();
    let (w_ends, _) = where_scanner.scan_line("    T: Display,");
    assert!(
        !w_ends,
        "Colon in where clause followed by comma must not end signature"
    );

    // Multi-line collection of foo<T: Clone>
    let lines_foo = [
        "pub fn foo<T: Clone>(",
        "    x: T,",
        ") -> T {",
        "    x",
        "}",
    ];
    let sig_foo = extract_signature_from_row(0, &lines_foo);
    assert_eq!(
        sig_foo.as_deref(),
        Some("pub fn foo<T: Clone>(x: T) -> T {")
    );

    // 4. Rust lifetime after whitespace (+ 'static, where 'a: 'b)
    let lines_static = ["fn f() -> impl Trait + 'static {", "    let x = 1;", "}"];
    let sig_static = extract_signature_from_row(0, &lines_static);
    assert_eq!(
        sig_static.as_deref(),
        Some("fn f() -> impl Trait + 'static {")
    );

    let lines_where = ["fn f<T>() -> T", "where 'a: 'b {", "    let x = 1;", "}"];
    let sig_where = extract_signature_from_row(0, &lines_where);
    assert_eq!(sig_where.as_deref(), Some("fn f<T>() -> T where 'a: 'b {"));

    // 5. Parameter list span detection with truncated parent scope having unclosed angle brackets
    let truncated_sig = "impl<T: Foo<Bar... > fn execute_operation(ctx: &mut Context, val: i32) -> Result<Status, Error> {";
    assert_eq!(
        find_parameter_list_span(truncated_sig, true),
        Some((41, 69))
    );
    let compacted_trunc = compact_signature_preview(truncated_sig, 95);
    assert!(
        compacted_trunc.contains("(ctx: &mut Context, ...)"),
        "Truncated prefix must allow candidate1 parameter compaction, got: {compacted_trunc}"
    );
    let compacted_trunc_short = compact_signature_preview(truncated_sig, 80);
    assert!(
        compacted_trunc_short.contains("(...)"),
        "Truncated prefix must allow candidate2 parameter compaction to (...), got: {compacted_trunc_short}"
    );
}

#[test]
fn test_container_attribute_only_has_none_sig_allowing_lookahead() {
    let rust_code = "\
mod tests {
    #[cfg(feature = \"tree-sitter\")]
    #[test]
    fn test_sample_case() {
        assert_eq!(1 + 1, 2);
    }
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(rust_code, None).unwrap();

    // Line 2 (1-indexed) is `#[cfg(feature = "tree-sitter")]`
    let (_, _, sig, _) = find_enclosing_definition_full(
        SupportedLanguage::Rust,
        &tree.root_node(),
        2,
        2,
        rust_code,
        None,
        0,
    );
    assert_eq!(
        sig, None,
        "Attribute-only range inside container must produce None sig so lookahead can expand to the definition below"
    );
}

#[test]
fn test_source_map_extensions() {
    assert!(is_lockfile_or_generated(Path::new("app.js.map")));
    assert!(is_lockfile_or_generated(Path::new("styles.css.map")));
    assert!(!is_lockfile_or_generated(Path::new("world.map")));
    assert!(!is_lockfile_or_generated(Path::new("game_level.map")));
}

#[test]
fn test_feedback_non_rust_single_quotes_and_rust_lifetime() {
    // TypeScript single quotes with spaces must not be swallowed as Rust lifetimes
    let ts_code = "\
function render(mode: 'dark mode' | 'light mode') {
    return mode;
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::TypeScript.language_fn())
        .unwrap();
    let tree = parser.parse(ts_code, None).unwrap();
    let (_, _, sig, _) = find_enclosing_definition_full(
        SupportedLanguage::TypeScript,
        &tree.root_node(),
        1,
        2,
        ts_code,
        None,
        0,
    );
    assert_eq!(
        sig.as_deref(),
        Some("function render(mode: 'dark mode' | 'light mode') {")
    );

    // Python single quotes with spaces
    let py_code = "\
def parse(x=('hello world', 'a')):
    pass
";
    let mut py_parser = Parser::new();
    py_parser
        .set_language(&SupportedLanguage::Python.language_fn())
        .unwrap();
    let py_tree = py_parser.parse(py_code, None).unwrap();
    let (_, _, py_sig, _) = find_enclosing_definition_full(
        SupportedLanguage::Python,
        &py_tree.root_node(),
        1,
        1,
        py_code,
        None,
        0,
    );
    assert_eq!(
        py_sig.as_deref(),
        Some("def parse(x=('hello world', 'a')):")
    );
}

#[test]
fn test_feedback_allow_hash_and_unspaced_comments() {
    assert_eq!(
        strip_line_comment_simple("def foo(x: int): #TODO", true, false),
        "def foo(x: int):"
    );
    assert_eq!(
        strip_line_comment_simple("def f(bar#comment", true, false),
        "def f(bar"
    );
    assert_eq!(strip_line_comment_simple("#comment only", true, false), "");

    // Python integer division and Bash substitution must NOT be stripped as comments when allow_hash is true
    assert_eq!(
        strip_line_comment_simple("def chunk(size: int = 100 // 2):", true, false),
        "def chunk(size: int = 100 // 2):"
    );
    assert_eq!(
        strip_line_comment_simple("echo ${var//a/b} # bash comment", true, false),
        "echo ${var//a/b}"
    );
    // But for languages with allow_hash = false (e.g. Rust, C++), // IS a comment
    assert_eq!(
        strip_line_comment_simple("let x = 100 // 2;", false, false),
        "let x = 100"
    );
}

#[test]
fn test_feedback_cfg_test_with_leading_attributes() {
    let multi_attr = "#[allow(dead_code)]\n#[cfg(test)]";
    assert!(is_cfg_test_attribute(multi_attr));
    assert!(is_rust_test_attr(multi_attr));

    let tab_attr = "#[inline]\n\t#[cfg(all(test, feature = \"extra\"))]";
    assert!(is_cfg_test_attribute(tab_attr));
    assert!(is_rust_test_attr(tab_attr));

    let reject = "#[allow(dead_code)]\n#[cfg(not(test))]";
    assert!(!is_cfg_test_attribute(reject));
    assert!(!is_rust_test_attr(reject));
}

#[test]
fn test_feedback_primary_child_priority_over_use_declaration() {
    let code = "\
use std::collections::HashMap;

pub fn execute_work() {
    let mut map = HashMap::new();
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();

    // Range spanning row 0 (use) to row 3 (pub fn execute_work)
    let (s, e, primary_child, def_count) =
        bounding_range_of_intersecting_children(SupportedLanguage::Rust, &tree.root_node(), 0, 3)
            .expect("expected children");
    assert_eq!(def_count, 1);
    assert_eq!(s, 1);
    assert_eq!(e, 5);
    assert!(primary_child.is_some());
    assert_eq!(
        primary_child.unwrap().kind(),
        "function_item",
        "primary_child must prefer definition over use_declaration"
    );
}

#[test]
fn test_feedback_top_level_tests_and_mod_tests_scope() {
    let code = "\
#[test]
fn test_alpha() {
    assert_eq!(1, 1);
}

#[test]
fn test_beta() {
    assert_eq!(2, 2);
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(code, None).unwrap();

    // Hunk spanning row 0 (start of test_alpha) through row 7 (test_beta)
    // Root node is source_file
    let (_s, _e, sig, is_clamped) = find_enclosing_definition_full(
        SupportedLanguage::Rust,
        &tree.root_node(),
        1,
        7,
        code,
        Some(Path::new("src/lib.rs")),
        0,
    );
    assert!(!is_clamped);
    assert!(sig.is_some());

    // mod tests itself
    let mod_code = "mod tests {\n    fn helper() {}\n}\n";
    let mod_tree = parser.parse(mod_code, None).unwrap();
    let mod_node = mod_tree.root_node().named_child(0).unwrap();
    let mod_lines: Vec<&str> = mod_code.lines().collect();
    assert!(is_test_scope(
        SupportedLanguage::Rust,
        &mod_node,
        Some(Path::new("src/lib.rs")),
        &mod_lines
    ));

    // Two top-level test functions whose combined span is 41..150 lines
    // Because they are in test scope, MAX_TEST_EXPANSION_LINES (40) applies, causing clamping.
    let mut long_test_code = String::new();
    long_test_code.push_str("#[test]\nfn test_first() {\n");
    for i in 0..22 {
        long_test_code.push_str(&format!("    let _a{i} = {i};\n"));
    }
    long_test_code.push_str("}\n\n#[test]\nfn test_second() {\n");
    for i in 0..22 {
        long_test_code.push_str(&format!("    let _b{i} = {i};\n"));
    }
    long_test_code.push_str("}\n");

    let long_tree = parser.parse(&long_test_code, None).unwrap();
    let total_lines = long_test_code.lines().count();
    assert!(total_lines > 40 && total_lines < 150);

    // A change spanning from inside test_first (line 24) to inside test_second (line 28).
    // Since it touches both top-level functions, the enclosing scope is the container (total span 51 lines):
    // If not test scope, max allowed is 150 lines, so 51 <= 150 would NOT clamp.
    // Because it IS test scope, MAX_TEST_EXPANSION_LINES (40) applies, so 51 > 40 clamps it!
    let (_s, _e, _sig, is_clamped) = find_enclosing_definition_full(
        SupportedLanguage::Rust,
        &long_tree.root_node(),
        24,
        28,
        &long_test_code,
        Some(Path::new("src/lib.rs")),
        0,
    );
    assert!(
        is_clamped,
        "Top-level tests spanning {total_lines} lines (> 40) must be clamped by MAX_TEST_EXPANSION_LINES"
    );
}

#[test]
fn test_feedback_param_compaction_edge_cases() {
    // 1. Quotes tracking in split_first_param_at_depth_zero
    let inner1 = "sep: str = \",\", flag: bool = True";
    assert_eq!(
        split_first_param_at_depth_zero(inner1, false),
        ("sep: str = \",\"", true)
    );

    let inner2 = "ch: char = ')', count: usize = 0";
    assert_eq!(
        split_first_param_at_depth_zero(inner2, false),
        ("ch: char = ')'", true)
    );

    // Rust lifetimes: single lifetime and multiple lifetimes
    let inner_rust1 = "node: &Node<'a>, x: usize, y: bool";
    assert_eq!(
        split_first_param_at_depth_zero(inner_rust1, true),
        ("node: &Node<'a>", true)
    );

    let inner_rust2 = "a: &'a str, b: &'b str, c: usize";
    assert_eq!(
        split_first_param_at_depth_zero(inner_rust2, true),
        ("a: &'a str", true)
    );

    // Backtick string and C++ digit separator
    let inner_js = "template: string = `val: ${x}`, count: number = 0";
    assert_eq!(
        split_first_param_at_depth_zero(inner_js, false),
        ("template: string = `val: ${x}`", true)
    );

    let inner_cpp = "val: int = 1'000, next: bool = false";
    assert_eq!(
        split_first_param_at_depth_zero(inner_cpp, false),
        ("val: int = 1'000", true)
    );

    // 2. find_parameter_list_span word boundary
    let typedef_c = "typedef struct Foo (Bar);";
    assert_eq!(find_parameter_list_span(typedef_c, false), Some((19, 23)));

    // Discriminative word boundary test with 2 candidates
    let multi_cand = "typedef_fn(a) def real(b, c)";
    let span = find_parameter_list_span(multi_cand, false).expect("expected param span");
    assert_eq!(&multi_cand[span.0..=span.1], "(b, c)");

    // 3. compact_signature_preview: empty parens and trailing comma
    let empty_parens = "pub fn initialize() -> Result<State, Error> {";
    let compacted_empty = compact_signature_preview(empty_parens, 30);
    assert!(
        !compacted_empty.contains("(...)"),
        "Empty parens must not be compacted into (...), got: {compacted_empty}"
    );

    // Trailing comma: max_chars set so candidate1 fits and trailing comma is stripped
    let trailing_comma = "pub fn execute(param: i32,) -> Result<State, Error> {";
    let compacted_tc = compact_signature_preview(trailing_comma, 52);
    assert!(
        !compacted_tc.contains(", ..."),
        "Trailing comma must not produce ', ...', got: {compacted_tc}"
    );
    assert!(
        compacted_tc.contains("execute(param: i32)"),
        "Parameter must be preserved without trailing comma, got: {compacted_tc}"
    );
}

#[test]
fn test_feedback_test_scope_and_annotation_boundaries() {
    let lines: Vec<&str> = Vec::new();
    let dummy_root = {
        let mut parser = Parser::new();
        parser
            .set_language(&SupportedLanguage::Rust.language_fn())
            .unwrap();
        parser.parse("fn foo() {}", None).unwrap()
    };
    let dummy_node = dummy_root.root_node();

    // Non-python files with test_ in name should NOT be test scope
    assert!(!is_test_scope(
        SupportedLanguage::Rust,
        &dummy_node,
        Some(Path::new("src/test_utils.rs")),
        &lines
    ));
    assert!(!is_test_scope(
        SupportedLanguage::Go,
        &dummy_node,
        Some(Path::new("src/test_runner.go")),
        &lines
    ));

    // Python file with test_ is test scope
    assert!(is_test_scope(
        SupportedLanguage::Python,
        &dummy_node,
        Some(Path::new("test_cases.py")),
        &lines
    ));
    assert!(is_test_scope(
        SupportedLanguage::Python,
        &dummy_node,
        Some(Path::new("tests/test_calc.py")),
        &lines
    ));

    // Test annotation word boundaries
    assert!(is_test_annotation_or_attr("@Test"));
    assert!(is_test_annotation_or_attr("@Test()"));
    assert!(is_test_annotation_or_attr(
        "@Test(expected = Exception.class)"
    ));
    assert!(!is_test_annotation_or_attr("@TestHelper"));
    assert!(!is_test_annotation_or_attr("@Testing"));
    assert!(is_test_annotation_or_attr("@RepeatedTest(5)"));
    assert!(is_test_annotation_or_attr("[Fact]"));
    assert!(is_test_annotation_or_attr("[Fact(Skip = \"reason\")]"));
    assert!(is_test_annotation_or_attr(
        "[Fact, Trait(\"Category\", \"Unit\")]"
    ));
    assert!(is_test_annotation_or_attr("[Test, Category(\"Fast\")]"));
    assert!(is_test_annotation_or_attr("[TestCase(1, 2)]"));
    assert!(is_test_annotation_or_attr("[TestMethod]"));
    assert!(is_test_annotation_or_attr("[TestFixture]"));
    assert!(!is_test_annotation_or_attr("[Factory]"));
}

#[test]
fn test_feedback_clamped_hunk_merge_and_full_coverage() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Case 1: Non-comment declaration on line 1 (unclamped: 1..=1) + clamped edit at val_2 (line 5 -> 2..=10).
    // They merge into a single hunk (2 <= 1 + 1) covering 1..=10, while full_end = 78 > 10 preserves [clamped].
    let mut initial = String::new();
    initial.push_str("const X: i32 = 1;\n");
    initial.push_str("fn large_function_to_clamp() {\n");
    for i in 0..75 {
        initial.push_str(&format!("    let val_{i} = {i};\n"));
    }
    initial.push_str("}\n");

    let file_path = repo.join("large.rs");
    std::fs::write(&file_path, &initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "large.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let mut mod_case1 = initial.clone();
    mod_case1 = mod_case1.replace("const X: i32 = 1;\n", "const X: i32 = 2;\n");
    mod_case1 = mod_case1.replace("    let val_2 = 2;\n", "    let val_2 = 999;\n");
    std::fs::write(&file_path, &mod_case1).unwrap();

    let mut warnings = Vec::new();
    let patch_case1 = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert_eq!(
        patch_case1.matches("\n@@ ").count(),
        1,
        "Unclamped const (1..=1) and clamped val_2 (2..=10) must merge into 1 hunk! Got:\n{patch_case1}"
    );
    assert!(
        patch_case1.contains("[clamped]"),
        "Merged hunk containing clamped large function and unclamped const must preserve [clamped]! Got:\n{patch_case1}"
    );

    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "large.rs"])
        .status()
        .unwrap();
    let mut apply_cmd1 = Command::new("git");
    apply_cmd1
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child1 = apply_cmd1
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child1
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch_case1.as_bytes())
        .unwrap();
    let out1 = child1.wait_with_output().unwrap();
    assert!(
        out1.status.success(),
        "git apply --check failed for Case 1: {}\nPatch:\n{patch_case1}",
        String::from_utf8_lossy(&out1.stderr)
    );

    // Case 2: Full coverage clears [clamped].
    // A 45-line #[test] function (fn body spans lines 2..=45, 44 lines > MAX_TEST_EXPANSION_LINES = 40).
    // Spaced edits each clamp to +-5 lines, and together merge into 2..=45 covering the full function.
    let mut test_fn = String::new();
    test_fn.push_str("#[test]\n");
    test_fn.push_str("fn test_full_coverage() {\n");
    for i in 0..42 {
        test_fn.push_str(&format!("    let step_{i} = {i};\n"));
    }
    test_fn.push_str("}\n");

    let test_path = repo.join("full_cov.rs");
    std::fs::write(&test_path, &test_fn).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "large.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "full_cov.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "add full_cov.rs"])
        .status()
        .unwrap();

    let mut mod_case2 = test_fn.clone();
    for idx in [3, 13, 23, 33, 38] {
        mod_case2 = mod_case2.replace(
            &format!("    let step_{idx} = {idx};\n"),
            &format!("    let step_{idx} = {};\n", idx + 1000),
        );
    }
    std::fs::write(&test_path, &mod_case2).unwrap();

    let mut warnings2 = Vec::new();
    let patch_case2 = generate_expanded_diff(repo, false, &mut warnings2).unwrap();
    assert_eq!(
        patch_case2.matches("\n@@ ").count(),
        1,
        "Spaced edits in 45-line #[test] fn must merge into 1 hunk! Got:\n{patch_case2}"
    );
    assert!(
        !patch_case2.contains("[clamped]"),
        "Merged hunk covering the entire function (2..=45) must clear [clamped]! Got:\n{patch_case2}"
    );

    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "full_cov.rs"])
        .status()
        .unwrap();
    let mut apply_cmd2 = Command::new("git");
    apply_cmd2
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child2 = apply_cmd2
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child2
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch_case2.as_bytes())
        .unwrap();
    let out2 = child2.wait_with_output().unwrap();
    assert!(
        out2.status.success(),
        "git apply --check failed for Case 2: {}\nPatch:\n{patch_case2}",
        String::from_utf8_lossy(&out2.stderr)
    );
}

#[test]
fn test_feedback_eof_python_node_not_falsely_clamped() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Python def ending at EOF with a trailing newline:
    // tree-sitter-python's function_definition end_position().row can point to new_lines.len().
    let initial = "def compute_sum(a: int, b: int) -> int:\n    total = a + b\n    return total\n";
    let file_path = repo.join("calc.py");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "calc.py"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let modified =
        "def compute_sum(a: int, b: int) -> int:\n    total = a * 2 + b\n    return total\n";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(
        patch.contains("def compute_sum(a: int, b: int) -> int:"),
        "Expected Python signature in patch, got:\n{patch}"
    );
    assert!(
        !patch.contains("[clamped]"),
        "Short EOF Python function must not be falsely marked [clamped]! Got:\n{patch}"
    );
}

#[test]
fn test_feedback_cfg_test_non_cfg_string_and_whitespace() {
    // Non-cfg attribute containing #[cfg(test)] or #[test] inside a string literal
    assert!(!is_cfg_test_attribute("#[doc = \"Use #[cfg(test)] here\"]"));
    assert!(!is_rust_test_attr("#[doc = \"Use #[cfg(test)] here\"]"));
    assert!(!is_rust_test_attr("#[doc = \"Use #[test] here\"]"));
    assert!(!is_cfg_test_attribute(
        "#[allow(unused, doc = \"a] #[cfg(test)]\")]"
    ));

    // Whitespace between `cfg` and `(` is valid Rust syntax
    assert!(is_cfg_test_attribute("#[cfg (test)]"));
    assert!(is_cfg_test_attribute("#![cfg (all(unix, test))]"));
    assert!(is_rust_test_attr("#[cfg (test)]"));

    // Non-cfg attribute followed by real #[cfg(test)]
    assert!(is_cfg_test_attribute("#[doc = \"note\"]\n#[cfg (test)]"));
}

#[test]
fn test_feedback_param_span_quotes_and_pub_boundary() {
    // 1. Default string containing ')' must not prematurely close parameter span
    let py_sig = "def parse(delim: str = \")\", opt: bool = False, extra_long_parameter_name: int = 0) -> list[str]:";
    let py_span = find_parameter_list_span(py_sig, false).expect("expected param span");
    assert_eq!(
        &py_sig[py_span.0..=py_span.1],
        "(delim: str = \")\", opt: bool = False, extra_long_parameter_name: int = 0)"
    );
    let py_compacted = compact_signature_preview_for_lang(py_sig, 70, SupportedLanguage::Python);
    assert_eq!(
        py_compacted,
        "def parse(delim: str = \")\", ...) -> list[str]:"
    );

    // 2. Decorator string containing '(' and '<' before function parameters
    let dec_sig = "@route(\"/items/(id)<int>\") def handle(req: Request, user: User, extra_long_arg: str) -> Response:";
    let dec_span = find_parameter_list_span(dec_sig, false).expect("expected param span");
    assert_eq!(
        &dec_sig[dec_span.0..=dec_span.1],
        "(req: Request, user: User, extra_long_arg: str)"
    );

    // 3. Rust char literal ')' alongside lifetimes
    let rust_sig = "pub fn parse_item<'a>(input: &'a str, delim: char = ')', extra_long_flag_parameter: bool) -> Result<&'a str, Error> {";
    let rust_span = find_parameter_list_span(rust_sig, true).expect("expected param span");
    assert_eq!(
        &rust_sig[rust_span.0..=rust_span.1],
        "(input: &'a str, delim: char = ')', extra_long_flag_parameter: bool)"
    );
    let rust_compacted = compact_signature_preview_for_lang(rust_sig, 90, SupportedLanguage::Rust);
    assert_eq!(
        rust_compacted,
        "pub fn parse_item<'a>(input: &'a str, ...) -> Result<&'a str, Error> {"
    );

    // 4. Function name ending in `pub` must not be confused with `pub(...)` visibility modifier
    let get_pub_sig = "def get_pub(first_arg: str, second_arg: str, third_arg: str) -> bool:";
    let get_pub_span = find_parameter_list_span(get_pub_sig, false).expect("expected param span");
    assert_eq!(
        &get_pub_sig[get_pub_span.0..=get_pub_span.1],
        "(first_arg: str, second_arg: str, third_arg: str)"
    );
}

#[test]
fn test_feedback_preamble_indentation_preserved() {
    let raw_stat = " src/input.rs            |    2 +\n src/tree_sitter_diff.rs | 2981 +++++\n 2 files changed\n";
    let trimmed = raw_stat.trim_matches(|c| c == '\r' || c == '\n');
    assert!(
        trimmed.starts_with(" src/input.rs"),
        "First line leading space must be preserved, got: {trimmed:?}"
    );
}

#[test]
fn test_review_attribute_lookahead_stops_at_non_definition_statement() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial = "\
use rayon::prelude::*;
const UNRELATED_CONST: i32 = 1;
fn unrelated_helper() -> i32 {
    let inner = 42;
    inner
}
";
    let file_path = repo.join("lib.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "lib.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Insert #[cfg(feature = "tree-sitter")] directly above `use rayon::prelude::*;` (no blank line)
    let modified = "\
#[cfg(feature = \"tree-sitter\")]
use rayon::prelude::*;
const UNRELATED_CONST: i32 = 1;
fn unrelated_helper() -> i32 {
    let inner = 42;
    inner
}
";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    let hunk_header = patch
        .lines()
        .find(|l| l.starts_with("@@ "))
        .expect("expected hunk header");
    assert_eq!(
        hunk_header, "@@ -1,3 +1,4 @@",
        "Lookahead over `use` statement must stop before pulling `const UNRELATED_CONST` or `fn unrelated_helper` into hunk signature! Got:\n{patch}"
    );
    assert!(
        !patch.contains("let inner = 42;"),
        "Lookahead over `use` statement must not expand unrelated function body! Got:\n{patch}"
    );

    // Multi-line attribute before a function should still look ahead cleanly:
    // 1) editing interior line of multi-line #[cfg(\n unix\n )] before fn only_unix()
    // 2) adding a comment above 2-line #[cfg(\n unix)] before fn only_unix2()
    let multiline_attr = "\
#[cfg(
    unix
)]
fn only_unix() -> i32 {
    let a = 1;
    let b = 2;
    let c = 3;
    a + b + c + 7
}

#[cfg(
    unix)]
fn only_unix2() -> i32 {
    let d = 4;
    let e = 5;
    let f = 6;
    d + e + f
}
";
    std::fs::write(&file_path, multiline_attr).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "lib.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "multiline attr"])
        .status()
        .unwrap();

    let multiline_mod = "\
#[cfg(
    not(windows)
)]
fn only_unix() -> i32 {
    let a = 1;
    let b = 2;
    let c = 3;
    a + b + c + 7
}

// platform-specific helper
#[cfg(
    unix)]
fn only_unix2() -> i32 {
    let d = 4;
    let e = 5;
    let f = 6;
    d + e + f
}
";
    std::fs::write(&file_path, multiline_mod).unwrap();
    let mut warnings2 = Vec::new();
    let patch2 = generate_expanded_diff(repo, false, &mut warnings2).unwrap();
    assert!(
        patch2.contains("@@ -2,8 +2,8 @@ fn only_unix() -> i32 {"),
        "Editing inside multi-line #[cfg(...)] should look ahead past `)]` and expand fn only_unix()! Got:\n{patch2}"
    );
    assert!(
        patch2.contains("@@ -11,8 +11,9 @@ fn only_unix2() -> i32 {"),
        "Comment above multi-line attribute should look ahead past non-`#[` attr continuation line and expand fn only_unix2()! Got:\n{patch2}"
    );
}

#[test]
fn test_review_summarize_omitted_chunk_git_binary_patch_stops_at_payload() {
    let git_binary_chunk = "\
diff --git a/bun.lockb b/bun.lockb
index 1234567..89abcdef 100644
GIT binary patch
literal 4096
zcmV+d5P1&~U|`^~2?+9<00000
zcmV+payload_line_should_be_omitted
";
    let summarized = summarize_omitted_chunk(git_binary_chunk, Path::new("bun.lockb"));
    assert!(summarized.contains("diff --git a/bun.lockb b/bun.lockb"));
    assert!(summarized.contains("[omitted: bun.lockb (binary)]"));
    assert!(
        !summarized.contains("literal 4096"),
        "Binary patch size metadata must not leak: {summarized}"
    );
    assert!(
        !summarized.contains("zcmV+"),
        "Base85 payload must not leak into header: {summarized}"
    );
}

#[test]
fn test_review_param_span_func_word_boundary_and_angle_operators() {
    // 1. Go receiver vs function ending in `func`
    let go_sig = "func (s *Server) Handle(req Request, res Response) error {";
    let go_span = find_parameter_list_span(go_sig, false).expect("go param span");
    assert_eq!(
        &go_sig[go_span.0..=go_span.1],
        "(req Request, res Response)"
    );

    let cpp_func_suffix = "void call_func(int a, int b) noexcept(true) {";
    let cpp_span = find_parameter_list_span(cpp_func_suffix, false).expect("cpp param span");
    assert_eq!(&cpp_func_suffix[cpp_span.0..=cpp_span.1], "(int a, int b)");

    let rust_func_suffix = "fn my_func(a: i32, b: i32) where F: Fn(i32) {";
    let rust_span = find_parameter_list_span(rust_func_suffix, true).expect("rust param span");
    assert_eq!(
        &rust_func_suffix[rust_span.0..=rust_span.1],
        "(a: i32, b: i32)"
    );

    // 2. C++ operator<< / operator< and shift << inside template args
    let cpp_stream = "std::ostream& operator<<(std::ostream& os, const Foo& f) {";
    let stream_span = find_parameter_list_span(cpp_stream, false).expect("operator<< span");
    assert_eq!(
        &cpp_stream[stream_span.0..=stream_span.1],
        "(std::ostream& os, const Foo& f)"
    );

    let cpp_lt = "bool operator<(const Foo& a, const Foo& b) {";
    let lt_span = find_parameter_list_span(cpp_lt, false).expect("operator< span");
    assert_eq!(
        &cpp_lt[lt_span.0..=lt_span.1],
        "(const Foo& a, const Foo& b)"
    );

    let cpp_shift_tmpl = "std::array<int, 1 << 4> make_arr(int x, int y) {";
    let shift_span = find_parameter_list_span(cpp_shift_tmpl, false).expect("template shift span");
    assert_eq!(
        &cpp_shift_tmpl[shift_span.0..=shift_span.1],
        "(int x, int y)"
    );

    // 3. Non-ASCII character immediately preceding `fn ` keyword
    let utf8_kw = "αfn(a) def real(b, c)";
    let utf8_span = find_parameter_list_span(utf8_kw, false).expect("utf8 boundary span");
    assert_eq!(&utf8_kw[utf8_span.0..=utf8_span.1], "(b, c)");

    // 4. split_first_param_at_depth_zero with `<`, `<=`, `<<` in default values
    assert_eq!(
        split_first_param_at_depth_zero("ok: bool = a < b, next: int = 1", false),
        ("ok: bool = a < b", true)
    );
    assert_eq!(
        split_first_param_at_depth_zero("ok: bool = a<b, next: int = 1", false),
        ("ok: bool = a<b", true)
    );
    assert_eq!(
        split_first_param_at_depth_zero("ok: bool = a <= b, next: int = 1", false),
        ("ok: bool = a <= b", true)
    );
    assert_eq!(
        split_first_param_at_depth_zero("mask: int = 1 << 4, next: int = 2", false),
        ("mask: int = 1 << 4", true)
    );
    assert_eq!(
        split_first_param_at_depth_zero("flag: bool = f(x < y), next: int = 2", false),
        ("flag: bool = f(x < y)", true)
    );

    // 5. Rust 1-tuple return type `-> (usize,)` must NOT lose its trailing comma
    let tuple_sig = "pub fn compute_pair(first_very_long_arg: SomeLongType, second_very_long_arg: OtherType) -> (usize,) {";
    let c1 = compact_signature_preview_for_lang(tuple_sig, 75, SupportedLanguage::Rust);
    assert!(
        c1.ends_with("-> (usize,) {"),
        "Candidate 1 must preserve 1-tuple return type, got: {c1}"
    );
    let c2 = compact_signature_preview_for_lang(tuple_sig, 45, SupportedLanguage::Rust);
    assert_eq!(c2, "pub fn compute_pair(...) -> (usize,) {");
}

#[test]
fn test_review_test_scope_mod_multiline_cfg_and_root_tests_dir() {
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();

    // 1. Root `__tests__/` directory
    let dummy_tree = parser.parse("fn helper() {}", None).unwrap();
    assert!(is_test_scope(
        SupportedLanguage::TypeScript,
        &dummy_tree.root_node(),
        Some(Path::new("__tests__/Button.test.ts")),
        &[]
    ));
    assert!(is_test_scope(
        SupportedLanguage::TypeScript,
        &dummy_tree.root_node(),
        Some(Path::new("__tests__/setup.ts")),
        &[]
    ));

    // 2. Custom-named mod_item with multi-line #[cfg(all(test, ...))]
    let mod_code = "\
#[cfg(all(
    test,
    feature = \"tree-sitter\"
))]
mod custom_helpers {
    fn helper_inside_multiline_cfg() {
        let x = 1;
    }
}
";
    let mod_lines: Vec<&str> = mod_code.lines().collect();
    let mod_tree = parser.parse(mod_code, None).unwrap();
    let fn_node = mod_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 5, column: 4 },
            tree_sitter::Point { row: 5, column: 25 },
        )
        .unwrap();
    assert!(
        is_test_scope(
            SupportedLanguage::Rust,
            &fn_node,
            Some(Path::new("src/lib.rs")),
            &mod_lines
        ),
        "Function inside custom-named mod with multi-line #[cfg(all(test, ...))] must be test scope"
    );
}

#[test]
fn test_review_cfg_test_attribute_not_handling() {
    assert!(is_cfg_test_attribute("#[cfg(all(not(test_util), test))]"));
    assert!(is_rust_test_attr("#[cfg(all(not(test_util), test))]"));

    assert!(!is_cfg_test_attribute("#[cfg(not(all(test, unix)))]"));
    assert!(!is_rust_test_attr("#[cfg(not(all(test, unix)))]"));
    assert!(!is_cfg_test_attribute(
        "#[cfg(all(not(any(test, fuzzing)), unix))]"
    ));
    assert!(!is_cfg_test_attribute("#[cfg(feature = \"test\")]"));
}

#[test]
fn test_review_char_literal_u_escape_and_raw_string() {
    assert_eq!(char_literal_len(r"'\u1234'"), Some(8));
    assert_eq!(char_literal_len(r"'\u{1f600}'"), Some(11));
    assert_eq!(char_literal_len(r"'\u1234\' // not comment'"), None);
    assert_eq!(char_literal_len(r"'\u{1234}\''"), None);

    assert_eq!(
        strip_line_comment_simple(
            r"def f(s: str = '\u1234\' // keep'): # comment",
            true,
            false
        ),
        r"def f(s: str = '\u1234\' // keep'):"
    );
    assert_eq!(
        strip_line_comment_simple(
            "function f(s = '\\u1234\\' // keep') { // real comment",
            false,
            false
        ),
        "function f(s = '\\u1234\\' // keep') {"
    );
    assert_eq!(
        strip_line_comment_simple(
            "const S: &str = r#\"a \" // b\"#; // real comment",
            false,
            true
        ),
        "const S: &str = r#\"a \" // b\"#;"
    );
}

#[test]
fn test_review_find_enclosing_definition_eof_clamping() {
    // 40-line Python test function ending at EOF with trailing newline:
    // tree-sitter-python reports end_position().row == 40 (so raw row+1 == 41 > 40 = MAX_TEST_EXPANSION_LINES).
    let mut py_code = String::from("def test_eof_exact_40():\n");
    for i in 1..=39 {
        py_code.push_str(&format!("    v_{i} = {i}\n"));
    }
    assert_eq!(py_code.lines().count(), 40);

    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Python.language_fn())
        .unwrap();
    let tree = parser.parse(&py_code, None).unwrap();

    let (exp_start, exp_end, sig, is_clamped) = find_enclosing_definition_full(
        SupportedLanguage::Python,
        &tree.root_node(),
        20,
        20,
        &py_code,
        Some(Path::new("tests/test_eof.py")),
        0,
    );
    assert_eq!(exp_start, 1);
    assert_eq!(exp_end, 40);
    assert!(
        !is_clamped,
        "40-line EOF test function must not be falsely clamped"
    );
    assert_eq!(sig.as_deref(), Some("def test_eof_exact_40():"));
}

#[test]
fn test_round2_pure_deletion_not_misattributed_and_git_apply() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Case 1: Deleting functions right after `fn alpha()` must NOT attribute the deletion to `fn alpha()`
    // or falsely label `fn alpha()` as `[clamped]`.
    let mut initial = String::from(
        "fn preamble_to_delete() -> i32 {\n    0\n}\n\nfn alpha() -> i32 {\n    let a = 1;\n    a + 1\n}\n\n",
    );
    for i in 0..30 {
        initial.push_str(&format!(
            "fn deleted_fn_{i}() -> i32 {{\n    let v = {i};\n    v * 2\n}}\n\n"
        ));
    }
    initial.push_str("fn omega() -> i32 {\n    999\n}\n");

    let file_path = repo.join("lib.rs");
    std::fs::write(&file_path, &initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "lib.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Delete `preamble_to_delete` at top-of-file AND all 30 `deleted_fn_*` after `alpha`
    let modified =
        "fn alpha() -> i32 {\n    let a = 1;\n    a + 1\n}\n\nfn omega() -> i32 {\n    999\n}\n";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    for line in patch.lines().filter(|l| l.starts_with("@@ ")) {
        assert!(
            !line.contains("fn alpha"),
            "Pure deletion hunk must NOT be misattributed to preceding `fn alpha`: {line}\nFull patch:\n{patch}"
        );
        assert!(
            !line.contains("[clamped]"),
            "Pure deletion hunk must NOT be marked [clamped]: {line}\nFull patch:\n{patch}"
        );
    }

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "lib.rs"])
        .status()
        .unwrap();
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git apply --check failed on pure deletion: {}\nPatch:\n{patch}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Case 2: Deleting a sibling method inside `impl Service` falls back to `impl Service` container scope
    let impl_initial = "\
struct Service;

impl Service {
    fn keep_first(&self) -> i32 {
        1
    }
    fn removed_middle(&self) -> i32 {
        let x = 10;
        let y = 20;
        x + y
    }
    fn keep_last(&self) -> i32 {
        3
    }
}
";
    std::fs::write(&file_path, impl_initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "lib.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "impl methods"])
        .status()
        .unwrap();

    let impl_modified = "\
struct Service;

impl Service {
    fn keep_first(&self) -> i32 {
        1
    }
    fn keep_last(&self) -> i32 {
        3
    }
}
";
    std::fs::write(&file_path, impl_modified).unwrap();
    let patch_impl = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    let impl_header = patch_impl
        .lines()
        .find(|l| l.starts_with("@@ "))
        .expect("expected hunk header");
    assert!(
        !impl_header.contains("keep_first"),
        "Sibling method deletion inside impl must not be attributed to keep_first: {impl_header}"
    );
    assert!(
        impl_header.contains("impl Service"),
        "Sibling method deletion inside impl should fall back to `impl Service`: {impl_header}"
    );
}

#[test]
fn test_round2_merged_hunk_preserves_clamped_signature_and_range_telemetry() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let mut initial = String::from("const MAGIC_OFFSET: i32 = 1;\n");
    initial.push_str("fn heavy_worker(input: i32) -> i32 {\n");
    for i in 1..=70 {
        initial.push_str(&format!("    let step_{i} = input + {i};\n"));
    }
    initial.push_str("    step_70\n}\n");

    let file_path = repo.join("worker.rs");
    std::fs::write(&file_path, &initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "worker.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Edit both the 1-line const and line 3 of heavy_worker so their expanded ranges merge
    let modified = initial
        .replace(
            "const MAGIC_OFFSET: i32 = 1;",
            "const MAGIC_OFFSET: i32 = 2;",
        )
        .replace("let step_2 = input + 2;", "let step_2 = input + 200;");
    std::fs::write(&file_path, &modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    let header = patch
        .lines()
        .find(|l| l.starts_with("@@ "))
        .expect("expected hunk header");

    assert!(
        header.contains("fn heavy_worker(input: i32) -> i32 {"),
        "Merged hunk with clamped function must display the clamped function signature! Got: {header}"
    );
    assert!(
        header.contains("(+1 more)"),
        "Merged hunk covering two definitions must include (+1 more)! Got: {header}"
    );
    assert!(
        header.contains("[clamped] (lines "),
        "Clamped hunk must include line range telemetry `[clamped] (lines X-Y of A-B)`! Got: {header}"
    );

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "worker.rs"])
        .status()
        .unwrap();
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git apply --check failed on merged clamped hunk: {}\nPatch:\n{patch}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn test_round2_pure_insertion_top_level_definition_has_surrounding_context_and_git_apply() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let initial = "\
fn first() -> i32 {
    1
}
fn third() -> i32 {
    3
}
";
    let file_path = repo.join("insert.rs");
    std::fs::write(&file_path, initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "insert.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Pure insertion of a new top-level definition with no blank lines around it
    let modified = "\
fn first() -> i32 {
    1
}
fn second() -> i32 {
    2
}
fn third() -> i32 {
    3
}
";
    std::fs::write(&file_path, modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    assert!(
        patch.contains(" }\n+fn second() -> i32 {"),
        "Pure top-level insertion must include preceding anchor context lines! Got:\n{patch}"
    );
    assert!(
        patch.contains("+}\n fn third() -> i32 {"),
        "Pure top-level insertion must include trailing anchor context lines! Got:\n{patch}"
    );

    // Verify git apply --check passes
    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "insert.rs"])
        .status()
        .unwrap();
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(patch.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git apply --check failed on pure top-level insertion: {}\nPatch:\n{patch}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn test_round2_signature_scanner_go_raw_csharp_verbatim_block_comments_ts_return_arrow() {
    // 1. Go raw backtick string ending in backslash (`C:\path\`) must not escape the closing backtick
    let go_lines = vec![
        "func OpenPath(defaultDir string /* `C:\\path\\` */,",
        "    secondArg int) error {",
        "    return nil",
        "}",
    ];
    let go_sig = extract_signature_from_row_for_lang(0, &go_lines, SupportedLanguage::Go).unwrap();
    assert_eq!(
        go_sig,
        "func OpenPath(defaultDir string, secondArg int) error {"
    );

    let go_raw_default = "func Load(p string = `C:\\path\\`, next int = 1) {";
    let go_span = find_parameter_list_span(go_raw_default, false).expect("expected param span");
    assert_eq!(
        &go_raw_default[go_span.0..=go_span.1],
        "(p string = `C:\\path\\`, next int = 1)"
    );
    let go_compacted =
        compact_signature_preview_for_lang(go_raw_default, 42, SupportedLanguage::Go);
    assert_eq!(go_compacted, "func Load(p string = `C:\\path\\`, ...) {");

    // 2. C# verbatim string `@"C:\path\"` and inline C# attribute `[HttpGet("api/v1")]`
    let cs_lines = vec![
        "[HttpGet(\"api/v1\")] public IActionResult GetFile(string dir = @\"C:\\path\\\",",
        "    int retryCount = 3) {",
        "    return Ok();",
        "}",
    ];
    let cs_sig =
        extract_signature_from_row_for_lang(0, &cs_lines, SupportedLanguage::CSharp).unwrap();
    assert_eq!(
        cs_sig,
        "public IActionResult GetFile(string dir = @\"C:\\path\\\", int retryCount = 3) {"
    );

    let cs_attr_inline =
        "[HttpGet(\"api/v1\")] public IActionResult Get(int id, string veryLongParameterNameForCompaction) {";
    let cs_span = find_parameter_list_span(cs_attr_inline, false).expect("csharp param span");
    assert_eq!(
        &cs_attr_inline[cs_span.0..=cs_span.1],
        "(int id, string veryLongParameterNameForCompaction)"
    );

    // 3. Signature internal block comment `/* don't { break */`
    let rust_block_comment_src = "\
pub fn process_items(
    first: i32, /* don't { break on apostrophe or brace */
    second: i32,
) -> i32 {
    first + second
}
";
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let tree = parser.parse(rust_block_comment_src, None).unwrap();
    let (_, _, sig_bc) = find_enclosing_definition(
        SupportedLanguage::Rust,
        &tree.root_node(),
        5,
        5,
        rust_block_comment_src,
    );
    assert_eq!(
        sig_bc.as_deref(),
        Some("pub fn process_items(first: i32, second: i32) -> i32 {")
    );

    // 4. TypeScript multiline function return type `: (x: string) =>\n Promise<void> {`
    let ts_src = "\
function createHandler(
    a: number
): (x: string) =>
    Promise<void> {
    return async (x) => {};
}
";
    let ts_lines: Vec<&str> = ts_src.lines().collect();
    let ts_row_sig =
        extract_signature_from_row_for_lang(0, &ts_lines, SupportedLanguage::TypeScript).unwrap();
    assert_eq!(
        ts_row_sig,
        "function createHandler(a: number): (x: string) => Promise<void> {"
    );

    // 5. C++ template with `<` comparison inside parens and without parens
    let cpp_tmpl1 = "template<int N = (1 < 2 ? 3 : 4)> void compute_val(int a, int b) {";
    let span1 = find_parameter_list_span(cpp_tmpl1, false).expect("cpp template ternary span");
    assert_eq!(&cpp_tmpl1[span1.0..=span1.1], "(int a, int b)");

    let cpp_tmpl2 = "template<bool B = 1 < 2> void compute_bool(int a, int b) {";
    let span2 = find_parameter_list_span(cpp_tmpl2, false).expect("cpp template spaced < span");
    assert_eq!(&cpp_tmpl2[span2.0..=span2.1], "(int a, int b)");

    // 6. Parent scope prefix coverage: TS abstract class / interface / namespace, JS class expr, Ruby class/module, Rust >5-line impl with `// {`
    parser
        .set_language(&SupportedLanguage::TypeScript.language_fn())
        .unwrap();
    let ts_scopes = "\
namespace AppUtils {
    abstract class BaseController {
        handleReq(x: number): number {
            return x + 1;
        }
    }
}
";
    let ts_tree = parser.parse(ts_scopes, None).unwrap();
    let (_, _, ts_sig) = find_enclosing_definition(
        SupportedLanguage::TypeScript,
        &ts_tree.root_node(),
        4,
        4,
        ts_scopes,
    );
    assert_eq!(
        ts_sig.as_deref(),
        Some("abstract class BaseController > handleReq(x: number): number {")
    );

    parser
        .set_language(&SupportedLanguage::JavaScript.language_fn())
        .unwrap();
    let js_class_expr = "\
const Widget = class {
    render() {
        return 42;
    }
};
";
    let js_tree = parser.parse(js_class_expr, None).unwrap();
    let (_, _, js_sig) = find_enclosing_definition(
        SupportedLanguage::JavaScript,
        &js_tree.root_node(),
        3,
        3,
        js_class_expr,
    );
    assert_eq!(js_sig.as_deref(), Some("class Widget > render() {"));

    parser
        .set_language(&SupportedLanguage::Ruby.language_fn())
        .unwrap();
    let rb_src = "\
module Billing
  class Invoice
    def total_cents(tax)
      100 + tax
    end
  end
end
";
    let rb_tree = parser.parse(rb_src, None).unwrap();
    let (_, _, rb_sig) =
        find_enclosing_definition(SupportedLanguage::Ruby, &rb_tree.root_node(), 4, 4, rb_src);
    assert_eq!(
        rb_sig.as_deref(),
        Some("class Invoice > def total_cents(tax)")
    );

    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let rust_long_impl = "\
impl<T> Processor<T>
// comment with fake brace {
where
    T: Clone,
    T: Send,
    T: Sync,
    T: std::fmt::Debug,
{
    fn run(&self) -> i32 {
        1
    }
}
";
    let rust_impl_tree = parser.parse(rust_long_impl, None).unwrap();
    let (_, _, rust_impl_sig) = find_enclosing_definition(
        SupportedLanguage::Rust,
        &rust_impl_tree.root_node(),
        10,
        10,
        rust_long_impl,
    );
    let sig_str = rust_impl_sig.unwrap();
    assert!(
        sig_str.starts_with("impl<T> Processor<T> where T: Clone")
            && sig_str.ends_with("> fn run(&self) -> i32 {"),
        "Expected comment-stripped multi-line impl parent scope, got: {sig_str}"
    );
}

#[test]
fn test_round2_test_scope_go_prod_ruby_cpp_maven_it_rust_fqn_multi_annotations() {
    let mut parser = Parser::new();

    // 1. Go production `func TestConnection()` in `src/conn.go` must NOT be treated as a test
    parser
        .set_language(&SupportedLanguage::Go.language_fn())
        .unwrap();
    let go_src = "package net\nfunc TestConnection() error {\n    return nil\n}\n";
    let go_lines: Vec<&str> = go_src.lines().collect();
    let go_tree = parser.parse(go_src, None).unwrap();
    let go_fn = go_tree.root_node().named_child(1).unwrap();
    assert!(
        !is_test_scope(
            SupportedLanguage::Go,
            &go_fn,
            Some(Path::new("src/conn.go")),
            &go_lines
        ),
        "Go production file `src/conn.go` with `func TestConnection` must NOT be flagged as test!"
    );
    assert!(
        is_test_scope(
            SupportedLanguage::Go,
            &go_fn,
            Some(Path::new("src/conn_test.go")),
            &go_lines
        ),
        "Go `src/conn_test.go` must be flagged as test!"
    );
    assert!(
        is_test_scope(SupportedLanguage::Go, &go_fn, None, &go_lines),
        "Go pathless snippet with `func TestConnection` should still use name heuristic!"
    );

    // 2. Ruby RSpec and Minitest detection
    parser
        .set_language(&SupportedLanguage::Ruby.language_fn())
        .unwrap();
    let rb_spec = "\
RSpec.describe Order do
  it 'calculates tax' do
    expect(1).to eq(1)
  end
end
";
    let rb_lines: Vec<&str> = rb_spec.lines().collect();
    let rb_tree = parser.parse(rb_spec, None).unwrap();
    let rb_inner = rb_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 2, column: 4 },
            tree_sitter::Point { row: 2, column: 10 },
        )
        .unwrap();
    assert!(is_test_scope(
        SupportedLanguage::Ruby,
        &rb_inner,
        Some(Path::new("app/order_check.rb")),
        &rb_lines
    ));
    assert!(is_test_file_path(Path::new("spec/models/order_spec.rb")));
    assert!(is_test_file_path(Path::new("order_test.rb")));

    // 3. C++ GoogleTest TEST_F / Catch2 TEST_CASE and `_unittest.cc`
    parser
        .set_language(&SupportedLanguage::Cpp.language_fn())
        .unwrap();
    let cpp_gtest = "TEST_F(ParserSuite, HandlesEmptyInput) {\n    ASSERT_TRUE(true);\n}\n";
    let cpp_lines: Vec<&str> = cpp_gtest.lines().collect();
    let cpp_tree = parser.parse(cpp_gtest, None).unwrap();
    let cpp_node = cpp_tree.root_node().named_child(0).unwrap();
    assert!(is_test_scope(
        SupportedLanguage::Cpp,
        &cpp_node,
        Some(Path::new("src/parser_check.cc")),
        &cpp_lines
    ));
    assert!(is_test_file_path(Path::new("src/parser_unittest.cc")));
    assert!(is_test_file_path(Path::new("src/parser_test.c")));

    // 4. Java Maven `*IT.java` camelCase check (no false positive on WAIT.java / AUDIT.java)
    assert!(is_test_file_path(Path::new("src/OrderIT.java")));
    assert!(is_test_file_path(Path::new("src/ApiV2IT.kt")));
    assert!(is_test_file_path(Path::new("src/OrderITCase.java")));
    assert!(is_test_file_path(Path::new("src/OrderSpec.java")));
    assert!(!is_test_file_path(Path::new("src/WAIT.java")));
    assert!(!is_test_file_path(Path::new("src/AUDIT.java")));
    assert!(!is_test_file_path(Path::new("src/LIMIT.java")));
    assert!(!is_test_file_path(Path::new("src/IT.java")));

    // 5. Rust generalized `*::test` attributes
    assert!(is_rust_test_attr("#[sqlx::test]"));
    assert!(is_rust_test_attr("#[test_log::test(tokio::test)]"));
    assert!(is_rust_test_attr("#[my_crate::custom::rstest]"));
    assert!(is_rust_test_attr("#[wasm_bindgen_test]"));
    assert!(!is_rust_test_attr("#[my_crate::test_helper]"));
    assert!(!is_rust_test_attr("#[test_utils::setup]"));

    // 6. Multiple annotations on same line & FQN annotations in Java / C#
    assert!(is_test_annotation_or_attr("@DisplayName(\" fast \") @Test"));
    assert!(is_test_annotation_or_attr("@org.junit.jupiter.api.Test"));
    assert!(is_test_annotation_or_attr(
        "[Trait(\"Category\", \"Unit\")] [Fact]"
    ));
    assert!(is_test_annotation_or_attr("[Xunit.Theory]"));
    assert!(!is_test_annotation_or_attr("@TestHelper"));
    assert!(!is_test_annotation_or_attr("[Factory]"));
}

#[test]
fn test_round2_extended_lookahead_and_attr_only_edit_clamping() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // 25-line function preceded by 6 stacked attributes (> old 3-line lookahead limit, > 15-line attr clamp cap)
    let mut initial = String::from(
        "// initial doc comment\n#[attr1]\n#[attr2]\n#[attr3]\n#[attr4]\n#[attr5]\n#[attr6]\npub fn big_annotated_fn(x: i32) -> i32 {\n",
    );
    for i in 1..=20 {
        initial.push_str(&format!("    let v_{i} = x + {i};\n"));
    }
    initial.push_str("    v_20\n}\n");

    let file_path = repo.join("annotated.rs");
    std::fs::write(&file_path, &initial).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "annotated.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Edit ONLY the top doc comment 7 lines above `pub fn big_annotated_fn`
    let modified = initial.replace("// initial doc comment", "// updated doc comment");
    std::fs::write(&file_path, &modified).unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    let header = patch
        .lines()
        .find(|l| l.starts_with("@@ "))
        .expect("expected hunk header");

    assert!(
        header.contains("pub fn big_annotated_fn(x: i32) -> i32 {"),
        "12-line lookahead must reach `pub fn big_annotated_fn` across 6 stacked attributes! Got:\n{patch}"
    );
    assert!(
        header.contains("[clamped] (lines "),
        "Attribute/comment-only edit on a >15-line function must clamp body expansion! Got:\n{patch}"
    );
    assert!(
        !patch.contains("let v_20 = x + 20;"),
        "Clamped attribute-only edit must not dump the entire 20-line function body! Got:\n{patch}"
    );
}

#[test]
fn test_round2_large_new_test_file_toc_and_cross_file_verbatim_move_dedup() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    // Commit an initial file with a 12-line function that will be moved verbatim to another file
    let moved_fn = "\
pub fn relocate_me(alpha: i32, beta: i32) -> i32 {
    let step1 = alpha + beta;
    let step2 = step1 * 2;
    let step3 = step2 - alpha;
    let step4 = step3 + 100;
    let step5 = step4 * 3;
    let step6 = step5 - beta;
    let step7 = step6 + 42;
    let step8 = step7 * 5;
    step8
}
";
    let old_src = format!("pub fn stay_here() -> i32 {{\n    1\n}}\n\n{moved_fn}");
    let existing_target = "pub fn existing_target_fn() -> i32 {\n    99\n}\n";
    std::fs::write(repo.join("old_mod.rs"), &old_src).unwrap();
    std::fs::write(repo.join("dest_mod.rs"), existing_target).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "."])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // 1. Move `relocate_me` from `old_mod.rs` into `dest_mod.rs` (modified file) AND `new_dest.rs` (new file)
    std::fs::write(
        repo.join("old_mod.rs"),
        "pub fn stay_here() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("dest_mod.rs"),
        format!("{existing_target}\n{moved_fn}"),
    )
    .unwrap();
    std::fs::write(
        repo.join("new_dest.rs"),
        format!("pub const HEADER: i32 = 1;\n\n{moved_fn}"),
    )
    .unwrap();

    // 2. Also add a large new test file (>= 200 lines)
    let mut big_test = String::new();
    for i in 0..25 {
        big_test.push_str(&format!(
            "#[test]\nfn test_case_{i}() {{\n    let a = {i};\n    let b = a + 1;\n    let c = b + 2;\n    let d = c + 3;\n    let e = d + 4;\n    assert_eq!(e, {i} + 10);\n}}\n\n"
        ));
    }
    let tests_dir = repo.join("tests");
    std::fs::create_dir_all(&tests_dir).unwrap();
    std::fs::write(tests_dir.join("mega_suite.rs"), &big_test).unwrap();

    // Stage everything so untracked files are included in diff
    Command::new("git")
        .current_dir(repo)
        .args(["add", "."])
        .status()
        .unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, true, &mut warnings).unwrap();

    // Check large new test file TOC summary
    assert!(
        patch.contains("[omitted: large new test file tests/mega_suite.rs"),
        "Expected large new test file TOC summary! Got:\n{patch}"
    );
    assert!(
        patch.contains("fn test_case_0() {"),
        "Expected TOC to list test function signatures! Got:\n{patch}"
    );
    assert!(
        !patch.contains("assert_eq!(e, 0 + 10);"),
        "Large new test file body must be omitted! Got:\n{patch}"
    );

    // Check verbatim move deduplication in both modified `dest_mod.rs` and new `new_dest.rs`
    let move_mentions = patch
        .matches(
            "+[moved verbatim from old_mod.rs: pub fn relocate_me(alpha: i32, beta: i32) -> i32 {",
        )
        .count();
    assert_eq!(
        move_mentions, 2,
        "Expected verbatim move summary in both modified dest_mod.rs and new new_dest.rs! Got:\n{patch}"
    );
}

#[test]
fn test_round3_dedent_utf8_ts_interface_ts_method_arrow_ruby_ivar_catch2_and_class_split_move() {
    // 1. `dedent_lines` with mixed ASCII indent and full-width Unicode space `\u{3000}` / NBSP `\u{00A0}` must not panic
    let utf8_lines = vec![
        "  fn a() {",
        "\u{3000}let fullwidth = 1;",
        "\u{00A0}let nbsp = 2;",
        "  }",
    ];
    let dedented = dedent_lines(&utf8_lines);
    assert!(dedented.contains("fn a() {"));
    assert!(dedented.contains("\u{3000}let fullwidth = 1;"));
    assert!(dedented.contains("\u{00A0}let nbsp = 2;"));

    // 2. TypeScript `interface` member edit must preserve `interface Foo > bar(): void;` signature & parent scope
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::TypeScript.language_fn())
        .unwrap();
    let ts_iface = "\
export interface UserRepository {
    findById(id: string): Promise<User>;
    saveUser(user: User): Promise<void>;
}
";
    let iface_tree = parser.parse(ts_iface, None).unwrap();
    let (_, _, iface_sig) = find_enclosing_definition(
        SupportedLanguage::TypeScript,
        &iface_tree.root_node(),
        2,
        2,
        ts_iface,
    );
    assert_eq!(
        iface_sig.as_deref(),
        Some("interface UserRepository > findById(id: string): Promise<User>;")
    );

    // 3. TypeScript class method (no `function` kw) with multiline function return type `: (x: string) =>\n Promise<void> {`
    // AND object literal property arrow function `handler: (x) =>\n x * 2,`
    let ts_method_lines = vec![
        "createHandler(a: number): (x: string) =>",
        "    Promise<void> {",
        "    return async (x) => {};",
        "}",
    ];
    let ts_method_sig =
        extract_signature_from_row_for_lang(0, &ts_method_lines, SupportedLanguage::TypeScript)
            .unwrap();
    assert_eq!(
        ts_method_sig,
        "createHandler(a: number): (x: string) => Promise<void> {"
    );

    let js_obj_arrow_lines = vec!["handler: (x) =>", "    x * 2,", "other: 123,"];
    let js_obj_sig =
        extract_signature_from_row_for_lang(0, &js_obj_arrow_lines, SupportedLanguage::JavaScript)
            .unwrap();
    assert_eq!(
        js_obj_sig, "handler: (x) =>",
        "Object property arrow function must stop at `=>` and not swallow body lines!"
    );

    // 4. Ruby instance variable `@total` must NOT be treated as an `@Annotation`
    let ruby_ivar_lines = vec!["    @total"];
    assert!(
        !is_attribute_or_comment_only(&ruby_ivar_lines, 1, 1, SupportedLanguage::Ruby),
        "Ruby `@total` instance variable must not be misclassified as an @Annotation!"
    );
    assert!(is_attribute_or_comment_only(
        &["@pytest.mark.parametrize"],
        1,
        1,
        SupportedLanguage::Python
    ));

    // 5. Bash `$@"..."` must not be misparsed as C# verbatim string
    let bash_lines = vec!["run_cmd() {", "    echo $@\"hello\"", "}"];
    let bash_sig =
        extract_signature_from_row_for_lang(0, &bash_lines, SupportedLanguage::Bash).unwrap();
    assert_eq!(bash_sig, "run_cmd() {");

    // 6. Catch2 `TEST_CASE("desc", "[tag]")` with string literal arguments
    parser
        .set_language(&SupportedLanguage::Cpp.language_fn())
        .unwrap();
    let catch2_src = "\
TEST_CASE(\"Factorials are computed\", \"[factorial]\") {
    REQUIRE(factorial(1) == 1);
    REQUIRE(factorial(2) == 2);
}
";
    let catch2_lines: Vec<&str> = catch2_src.lines().collect();
    let catch2_tree = parser.parse(catch2_src, None).unwrap();
    let catch2_inner = catch2_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 1, column: 4 },
            tree_sitter::Point { row: 1, column: 20 },
        )
        .unwrap();
    assert!(
        is_test_scope(
            SupportedLanguage::Cpp,
            &catch2_inner,
            Some(Path::new("src/math_checks.cpp")),
            &catch2_lines
        ),
        "Catch2 TEST_CASE(\"desc\", \"[tag]\") with string literal args must be detected as test scope!"
    );

    // 7. Class-splitting refactoring: moving a method out of `class BigService` into a new `class SplitService`
    // that also has a new method must still deduplicate the moved method!
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);

    let moved_method = "\
    pub fn compute_invoice_totals(&self, base: i32, tax: i32) -> i32 {
        let s1 = base + tax;
        let s2 = s1 * 2;
        let s3 = s2 - base;
        let s4 = s3 + 100;
        let s5 = s4 * 3;
        let s6 = s5 - tax;
        let s7 = s6 + 42;
        let s8 = s7 * 10;
        s8
    }";
    let initial_service = format!(
        "struct BigService;\nimpl BigService {{\n    pub fn keep_here(&self) -> i32 {{\n        1\n    }}\n\n{moved_method}\n}}\n"
    );
    std::fs::write(repo.join("service.rs"), &initial_service).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "service.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    // Remove `compute_invoice_totals` from `BigService` and place it inside `impl SplitService` alongside a brand-new method
    std::fs::write(
        repo.join("service.rs"),
        "struct BigService;\nimpl BigService {\n    pub fn keep_here(&self) -> i32 {\n        1\n    }\n}\n",
    )
    .unwrap();
    let new_split_file = format!(
        "struct SplitService;\nimpl SplitService {{\n{moved_method}\n\n    pub fn brand_new_method(&self) -> i32 {{\n        777\n    }}\n}}\n"
    );
    std::fs::write(repo.join("split_service.rs"), &new_split_file).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "."])
        .status()
        .unwrap();

    let mut warnings = Vec::new();
    let patch = generate_expanded_diff(repo, true, &mut warnings).unwrap();
    assert!(
        patch.contains("+[moved verbatim from service.rs:")
            && patch.contains("compute_invoice_totals"),
        "Method moved into a new impl block alongside new methods must still be deduplicated! Got:\n{patch}"
    );
    assert!(
        patch.contains("+    pub fn brand_new_method(&self) -> i32 {"),
        "Non-moved method in the new impl block must still be emitted in full! Got:\n{patch}"
    );
}

#[test]
fn test_feedback_fixes_cpp_preprocessor_ast_test_scope_and_direct_ast_sig_compaction() {
    // 1. C/C++ and C# preprocessor directives with whitespace after `#` must NOT be treated as comments
    for line in [
        "# include <stdio.h>",
        "# define FOO 1",
        "# ifdef DEBUG",
        "# endif",
        "#",
    ] {
        assert!(
            !is_attribute_or_comment_only(&[line], 1, 1, SupportedLanguage::C),
            "C preprocessor line {line:?} must not be classified as comment/attr"
        );
        assert!(
            !is_attribute_or_comment_only(&[line], 1, 1, SupportedLanguage::Cpp),
            "C++ preprocessor line {line:?} must not be classified as comment/attr"
        );
    }
    assert!(!is_attribute_or_comment_only(
        &["# if DEBUG"],
        1,
        1,
        SupportedLanguage::CSharp
    ));
    assert!(is_attribute_or_comment_only(
        &["# python comment"],
        1,
        1,
        SupportedLanguage::Python
    ));

    // 2. AST-first Rust `is_test_scope`: handles inner `#![cfg(test)]` and ignores `#[test]` inside raw strings
    let mut parser = Parser::new();
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let rust_src = "\
const DOC: &str = r#\"
#[test]
\"#;
fn production_fn() {
    let x = 1;
}

mod inner_suite {
    #![cfg(test)]
    fn helper_inside_inner_cfg_test() {
        let y = 2;
    }
}
";
    let rust_lines: Vec<&str> = rust_src.lines().collect();
    let rust_tree = parser.parse(rust_src, None).unwrap();
    let prod_fn = rust_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 4, column: 4 },
            tree_sitter::Point { row: 4, column: 10 },
        )
        .unwrap();
    let prod_fn_item = prod_fn.parent().unwrap().parent().unwrap();
    assert_eq!(prod_fn_item.kind(), "function_item");
    assert!(
        !is_test_scope(
            SupportedLanguage::Rust,
            &prod_fn_item,
            Some(Path::new("src/app.rs")),
            &rust_lines
        ),
        "Raw string containing #[test] before fn must not trigger test scope"
    );

    let inner_fn_stmt = rust_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 10, column: 8 },
            tree_sitter::Point {
                row: 10,
                column: 15,
            },
        )
        .unwrap();
    assert!(
        is_test_scope(
            SupportedLanguage::Rust,
            &inner_fn_stmt,
            Some(Path::new("src/app.rs")),
            &rust_lines
        ),
        "Module with inner #![cfg(test)] must be detected via AST"
    );

    // 3. AST-first Java and C# `is_test_scope`
    parser
        .set_language(&SupportedLanguage::Java.language_fn())
        .unwrap();
    let java_src = "\
class BillingService {
    @org.junit.jupiter.api.Test
    void verifyTotals() {
        int x = 1;
    }
}
";
    let java_lines: Vec<&str> = java_src.lines().collect();
    let java_tree = parser.parse(java_src, None).unwrap();
    let java_stmt = java_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 3, column: 8 },
            tree_sitter::Point { row: 3, column: 15 },
        )
        .unwrap();
    assert!(is_test_scope(
        SupportedLanguage::Java,
        &java_stmt,
        Some(Path::new("src/BillingService.java")),
        &java_lines
    ));

    parser
        .set_language(&SupportedLanguage::CSharp.language_fn())
        .unwrap();
    let cs_src = "\
class BillingChecks {
    [Xunit.Fact]
    public void VerifyTotals() {
        int x = 1;
    }
}
";
    let cs_lines: Vec<&str> = cs_src.lines().collect();
    let cs_tree = parser.parse(cs_src, None).unwrap();
    let cs_stmt = cs_tree
        .root_node()
        .named_descendant_for_point_range(
            tree_sitter::Point { row: 3, column: 8 },
            tree_sitter::Point { row: 3, column: 15 },
        )
        .unwrap();
    assert!(is_test_scope(
        SupportedLanguage::CSharp,
        &cs_stmt,
        Some(Path::new("src/BillingChecks.cs")),
        &cs_lines
    ));

    // 4. Direct AST byte-slice signature compaction handles duplicate `(a: i32, b: i32)` spans
    //    in generic bounds and JS/TS exported arrow functions without string reverse-search.
    parser
        .set_language(&SupportedLanguage::Rust.language_fn())
        .unwrap();
    let dup_param_src = "\
pub fn orchestrate_with_callback<F: Fn(alpha: i32, beta: i32) -> i32>(alpha: i32, beta: i32, gamma: i32, delta: i32) -> Result<i32, String> {
    Ok(alpha + beta + gamma + delta)
}
";
    let dup_lines: Vec<&str> = dup_param_src.lines().collect();
    let dup_tree = parser.parse(dup_param_src, None).unwrap();
    let dup_fn = dup_tree.root_node().named_child(0).unwrap();
    let dup_raw = extract_signature(SupportedLanguage::Rust, &dup_fn, &dup_lines, dup_param_src);
    let dup_compact = compact_definition_signature_with_ast(
        SupportedLanguage::Rust,
        &dup_fn,
        None,
        dup_raw.as_deref(),
        dup_param_src,
        120,
    )
    .unwrap();
    assert!(
        dup_compact.contains("Fn(alpha: i32, beta: i32) -> i32>(alpha: i32, ...) -> Result<i32, String> {"),
        "Direct AST slice compaction must compact function params, not generic bound params! Got: {dup_compact}"
    );

    parser
        .set_language(&SupportedLanguage::TypeScript.language_fn())
        .unwrap();
    let ts_arrow_src = "\
export const dispatchPipelineRequest = (requestPayload: IncomingRequest, responseContext: ResponseContext, nextMiddleware: NextFn): Promise<void> => {
    return Promise.resolve();
};
";
    let ts_lines: Vec<&str> = ts_arrow_src.lines().collect();
    let ts_tree = parser.parse(ts_arrow_src, None).unwrap();
    let ts_export = ts_tree.root_node().named_child(0).unwrap();
    let ts_raw = extract_signature(
        SupportedLanguage::TypeScript,
        &ts_export,
        &ts_lines,
        ts_arrow_src,
    );
    let ts_compact = compact_definition_signature_with_ast(
        SupportedLanguage::TypeScript,
        &ts_export,
        None,
        ts_raw.as_deref(),
        ts_arrow_src,
        115,
    )
    .unwrap();
    assert!(
        ts_compact.starts_with("export const dispatchPipelineRequest = (")
            && ts_compact.ends_with(": Promise<void> => {"),
        "Exported TS arrow function should compact cleanly via AST slices, got: {ts_compact}"
    );

    // 5. `trim_unified_diff_context` splits wide hunks cleanly and produces a git-applyable -U3 patch
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    let mut orig_src = String::from("fn wide_function(mut v: i32) -> i32 {\n");
    for i in 1..=30 {
        orig_src.push_str(&format!("    v += {i};\n"));
    }
    orig_src.push_str("    v\n}\n");
    std::fs::write(repo.join("wide.rs"), &orig_src).unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["add", "wide.rs"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "init"])
        .status()
        .unwrap();

    let mod_src = orig_src
        .replace("    v += 4;\n", "    v += 400;\n")
        .replace("    v += 26;\n", "    v += 2600;\n");
    std::fs::write(repo.join("wide.rs"), &mod_src).unwrap();

    let mut warnings = Vec::new();
    let expanded = generate_expanded_diff(repo, false, &mut warnings).unwrap();
    let diff_start = expanded.find("diff --git ").unwrap();
    let chunk = &expanded[diff_start..];
    let trimmed =
        trim_unified_diff_context(chunk, 3).expect("expected wide AST hunk to trim to -U3");
    assert_eq!(
        trimmed.matches("\n@@ ").count() + usize::from(trimmed.starts_with("@@ ")),
        2,
        "Two distant edits inside wide_function should split into 2 -U3 hunks:\n{trimmed}"
    );
    assert!(
        trimmed.contains("fn wide_function(mut v: i32) -> i32 {"),
        "Trimmed -U3 hunks must preserve AST signature header:\n{trimmed}"
    );

    Command::new("git")
        .current_dir(repo)
        .args(["checkout", "wide.rs"])
        .status()
        .unwrap();
    let mut apply_cmd = Command::new("git");
    apply_cmd
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false", "apply", "--check", "-"]);
    let mut child = apply_cmd
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(trimmed.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git apply --check failed on trimmed -U3 patch: {}\n{trimmed}",
        String::from_utf8_lossy(&out.stderr)
    );
}
