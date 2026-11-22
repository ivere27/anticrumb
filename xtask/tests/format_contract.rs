use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn cargo_bin() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn check_formatting(root: &Path) -> Result<(), String> {
    let cargo = cargo_bin();
    let output = Command::new(&cargo)
        .current_dir(root)
        .args(["fmt", "--all", "--check"])
        .output()
        .map_err(|e| {
            format!(
                "failed to run {} fmt --all --check: {e}",
                cargo.to_string_lossy()
            )
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "cargo fmt --all --check failed in {} (status: {}); run `cargo fmt --all` to fix:\n{}{}",
        root.display(),
        output.status,
        stdout,
        stderr,
    ))
}

#[test]
fn workspace_rust_formatting_matches_rustfmt() {
    if let Err(err) = check_formatting(&workspace_root()) {
        panic!("{err}");
    }
}

#[test]
fn formatting_check_rejects_unformatted_rust_and_accepts_formatted_rust() {
    let temp = TempDir::new().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"fmt_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("lib.rs"), "pub fn   add(a:i32,b:i32)->i32{a+b}\n").unwrap();

    let err = check_formatting(temp.path()).expect_err("unformatted code must fail fmt check");
    assert!(
        err.contains("Diff in"),
        "expected diff output from cargo fmt --all --check, got:\n{err}"
    );

    let status = Command::new(cargo_bin())
        .current_dir(temp.path())
        .args(["fmt", "--all"])
        .status()
        .expect("run cargo fmt --all on fixture");
    assert!(status.success(), "cargo fmt --all failed on fixture");

    check_formatting(temp.path()).expect("formatted fixture must pass fmt check");
}
