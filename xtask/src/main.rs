use std::path::PathBuf;
use xtask::build::Action;

fn workspace_root() -> PathBuf {
    if let Some(root) = std::env::var_os("ANTICRUMB_ROOT") {
        return PathBuf::from(root);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        eprintln!(
            "Usage: cargo run --locked -q -p xtask -- <build|package|check|test|transports|tui|macos-cross|proto|publish|release-ci> [options]"
        );
        std::process::exit(2);
    };

    let root = workspace_root();
    let code = match command.as_str() {
        "build" => xtask::build::run_cli(&root, Action::Build, rest),
        "package" => xtask::build::run_cli(&root, Action::Package, rest),
        "check" => xtask::build::run_cli(&root, Action::Check, rest),
        "test" => xtask::build::run_cli(&root, Action::Test, rest),
        "transports" => xtask::build::run_cli(&root, Action::Transports, rest),
        "tui" => xtask::build::run_cli(&root, Action::Tui, rest),
        "macos-cross" => xtask::build::run_macos_cross(&root, rest),
        "proto" => xtask::generate::run_cli(&root),
        "publish" => xtask::publish::run_cli(&root, rest),
        "release-ci" => xtask::publish::run_ci_cli(&root, rest),
        other => {
            eprintln!("Unknown xtask command: {other}");
            2
        }
    };
    std::process::exit(code);
}
