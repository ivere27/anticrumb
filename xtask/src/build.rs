use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Build,
    Package,
    Check,
    Test,
    Transports,
    Tui,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Dev,
    Release,
}

impl Profile {
    fn dir_name(self) -> &'static str {
        match self {
            Profile::Dev => "debug",
            Profile::Release => "release",
        }
    }
}

fn usage() {
    eprintln!(
        "Usage: cargo run --locked -q -p xtask -- <build|package|check|test|transports|tui> [--target TRIPLE] [--profile dev|release] [--jobs N]"
    );
}

fn jobs_for_cpus(cpus: usize) -> usize {
    cpus.saturating_sub(2).max(1)
}

fn default_build_jobs() -> usize {
    jobs_for_cpus(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
}

fn cargo_bin() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn rustc_bin() -> String {
    env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string())
}

fn run_checked(
    program: &str,
    args: &[&str],
    root: &Path,
    extra_env: &[(&str, &str)],
) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(root);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !status.success() {
        return Err(format!(
            "{program} {} exited with status {}",
            args.join(" "),
            status.code().unwrap_or(1)
        ));
    }
    Ok(())
}

fn host_target(root: &Path) -> Result<String, String> {
    let rustc = rustc_bin();
    let out = Command::new(&rustc)
        .arg("-vV")
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to run {rustc} -vV: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{rustc} -vV failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.trim().to_string());
        }
    }
    Err("Failed to determine Rust host target via rustc -vV".to_string())
}

fn copy_dir_deref(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let mut entries: Vec<PathBuf> = fs::read_dir(src)
        .map_err(|e| format!("read_dir {}: {e}", src.display()))?
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .ok_or_else(|| format!("invalid path {}", path.display()))?;
        let target = dst.join(name);
        let meta = fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            copy_dir_deref(&path, &target)?;
        } else {
            fs::copy(&path, &target)
                .map_err(|e| format!("copy {} -> {}: {e}", path.display(), target.display()))?;
        }
    }
    Ok(())
}

fn collect_tree(
    base: &Path,
    dir: &Path,
    dirs: &mut Vec<String>,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("read_dir {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for path in entries {
        let rel = path
            .strip_prefix(base)
            .map_err(|e| format!("strip_prefix {}: {e}", path.display()))?;
        let rel_unix = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let meta = fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            dirs.push(rel_unix);
            collect_tree(base, &path, dirs, files)?;
        } else {
            files.insert(rel_unix, path);
        }
    }
    Ok(())
}

fn write_tar_gz_package(
    archive_path: &Path,
    package_name: &str,
    destination: &Path,
    executable: &str,
) -> Result<(), String> {
    let mut dirs = Vec::new();
    let mut files = BTreeMap::new();
    collect_tree(destination, destination, &mut dirs, &mut files)?;

    let file = File::create(archive_path)
        .map_err(|e| format!("create {}: {e}", archive_path.display()))?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(gz);

    let append_dir =
        |builder: &mut tar::Builder<GzEncoder<File>>, arc_path: &str| -> Result<(), String> {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header
                .set_path(arc_path)
                .map_err(|e| format!("tar dir path {arc_path}: {e}"))?;
            header.set_cksum();
            builder
                .append(&header, std::io::empty())
                .map_err(|e| format!("append tar dir {arc_path}: {e}"))
        };

    append_dir(&mut builder, &format!("{package_name}/"))?;
    for d in dirs {
        append_dir(&mut builder, &format!("{package_name}/{d}/"))?;
    }
    for (rel, src) in files {
        let arc_path = format!("{package_name}/{rel}");
        let bytes = fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
        let mode = if rel == executable { 0o755 } else { 0o644 };
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(mode);
        header.set_size(bytes.len() as u64);
        header
            .set_path(&arc_path)
            .map_err(|e| format!("tar file path {arc_path}: {e}"))?;
        header.set_cksum();
        builder
            .append(&header, bytes.as_slice())
            .map_err(|e| format!("append tar file {arc_path}: {e}"))?;
    }

    let gz = builder
        .into_inner()
        .map_err(|e| format!("finalize tar {}: {e}", archive_path.display()))?;
    gz.finish()
        .map_err(|e| format!("finish gzip {}: {e}", archive_path.display()))?;
    Ok(())
}

fn write_zip_package(
    archive_path: &Path,
    package_name: &str,
    destination: &Path,
    executable: &str,
) -> Result<(), String> {
    let mut dirs = Vec::new();
    let mut files = BTreeMap::new();
    collect_tree(destination, destination, &mut dirs, &mut files)?;

    let file = File::create(archive_path)
        .map_err(|e| format!("create {}: {e}", archive_path.display()))?;
    let mut writer = ZipWriter::new(file);
    let dir_opts = SimpleFileOptions::default().unix_permissions(0o755);
    writer
        .add_directory(format!("{package_name}/"), dir_opts)
        .map_err(|e| format!("zip root dir: {e}"))?;
    for d in dirs {
        writer
            .add_directory(format!("{package_name}/{d}/"), dir_opts)
            .map_err(|e| format!("zip dir {d}: {e}"))?;
    }
    for (rel, src) in files {
        let mode = if rel == executable {
            0o100755
        } else {
            0o100644
        };
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(mode);
        writer
            .start_file(format!("{package_name}/{rel}"), opts)
            .map_err(|e| format!("zip start {rel}: {e}"))?;
        let bytes = fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
        writer
            .write_all(&bytes)
            .map_err(|e| format!("zip write {rel}: {e}"))?;
    }
    writer
        .finish()
        .map_err(|e| format!("zip finish {}: {e}", archive_path.display()))?;
    Ok(())
}

pub fn execute(
    root: &Path,
    action: Action,
    target_opt: Option<&str>,
    profile: Profile,
    extra_env: &[(&str, &str)],
) -> Result<(), String> {
    let jobs = (action == Action::Build).then(default_build_jobs);
    execute_with_jobs(root, action, target_opt, profile, jobs, extra_env)
}

pub fn execute_with_jobs(
    root: &Path,
    action: Action,
    target_opt: Option<&str>,
    profile: Profile,
    jobs: Option<usize>,
    extra_env: &[(&str, &str)],
) -> Result<(), String> {
    let cargo = cargo_bin();
    match action {
        Action::Check => {
            run_checked(&cargo, &["fmt", "--all", "--check"], root, extra_env)?;
            run_checked(
                &cargo,
                &[
                    "clippy",
                    "--locked",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
                root,
                extra_env,
            )?;
            return Ok(());
        }
        Action::Test => {
            run_checked(
                &cargo,
                &["test", "--locked", "-p", "anticrumb", "--all-targets"],
                root,
                extra_env,
            )?;
            // Avoid rebuilding target/debug/xtask(.exe) via --workspace feature
            // unification or --bins while this process is executing from it on Windows.
            run_checked(
                &cargo,
                &["test", "--locked", "-p", "xtask", "--lib", "--tests"],
                root,
                extra_env,
            )?;
            return Ok(());
        }
        _ => {}
    }

    let host = host_target(root)?;
    let target = target_opt.unwrap_or(&host).to_string();
    let profile_dir = profile.dir_name();

    if matches!(action, Action::Transports | Action::Tui) && target != host {
        return Err("transport and PTY tests must run on their native target".to_string());
    }

    let target_dir_raw = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let target_dir = if target_dir_raw.is_absolute() {
        target_dir_raw
    } else {
        root.join(target_dir_raw)
    };
    let binaries = target_dir.join(&target).join(profile_dir);

    let jobs_str = jobs.map(|n| n.max(1).to_string());
    let mut build_args = vec!["build", "--locked", "--target", &target];
    if profile == Profile::Release {
        build_args.push("--release");
    }
    if let Some(ref jobs_val) = jobs_str {
        build_args.extend(["-j", jobs_val.as_str()]);
    }
    build_args.extend(["--bin", "anticrumb"]);
    if action == Action::Tui {
        build_args.extend(["--example", "tui_fixture"]);
    }
    run_checked(&cargo, &build_args, root, extra_env)?;

    let windows = target.contains("windows");
    let executable = if windows {
        "anticrumb.exe"
    } else {
        "anticrumb"
    };

    match action {
        Action::Build => {
            println!("{}", binaries.join(executable).display());
            Ok(())
        }
        Action::Transports => {
            let mut test_args = vec!["test", "--locked", "--target", &target];
            if profile == Profile::Release {
                test_args.push("--release");
            }
            test_args.extend([
                "--test",
                "rpc_contract",
                "static_module_unary_stream_and_cancellation_use_generated_api",
                "--",
                "--test-threads=1",
            ]);
            run_checked(&cargo, &test_args, root, extra_env)
        }
        Action::Tui => {
            if cfg!(windows) || target.contains("windows") {
                return Err(
                    "PTY tests require Linux or macOS; Rust tests cover Windows terminal state"
                        .to_string(),
                );
            }
            let fixture = binaries.join("examples").join("tui_fixture");
            let fixture_str = fixture.to_string_lossy().into_owned();
            let mut combined_env = extra_env.to_vec();
            combined_env.push(("ANTICRUMB_TUI_FIXTURE", &fixture_str));
            let mut test_args = vec!["test", "--locked", "--target", &target];
            if profile == Profile::Release {
                test_args.push("--release");
            }
            test_args.extend(["--test", "tui_contract"]);
            run_checked(&cargo, &test_args, root, &combined_env)
        }
        Action::Package => {
            let package_name = format!("anticrumb-{target}");
            let dist_dir = root.join("dist");
            let destination = dist_dir.join(&package_name);
            if destination.exists() {
                fs::remove_dir_all(&destination)
                    .map_err(|e| format!("remove {}: {e}", destination.display()))?;
            }
            fs::create_dir_all(&destination)
                .map_err(|e| format!("mkdir {}: {e}", destination.display()))?;
            let bin_src = binaries.join(executable);
            let bin_dst = destination.join(executable);
            fs::copy(&bin_src, &bin_dst)
                .map_err(|e| format!("copy {} -> {}: {e}", bin_src.display(), bin_dst.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&bin_dst, fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("chmod {}: {e}", bin_dst.display()))?;
            }
            fs::copy(root.join("README.md"), destination.join("README.md"))
                .map_err(|e| format!("copy README.md: {e}"))?;
            copy_dir_deref(&root.join("docs"), &destination.join("docs"))?;
            fs::copy(root.join("LICENSE"), destination.join("LICENSE"))
                .map_err(|e| format!("copy LICENSE: {e}"))?;

            let archive = if windows {
                let p = dist_dir.join(format!("{package_name}.zip"));
                let _ = fs::remove_file(&p);
                write_zip_package(&p, &package_name, &destination, executable)?;
                p
            } else {
                let p = dist_dir.join(format!("{package_name}.tar.gz"));
                let _ = fs::remove_file(&p);
                write_tar_gz_package(&p, &package_name, &destination, executable)?;
                p
            };
            println!("{}", archive.display());
            Ok(())
        }
        Action::Check | Action::Test => unreachable!(),
    }
}

fn parse_jobs_arg(raw: &str) -> Result<usize, String> {
    let parsed: usize = raw
        .parse()
        .map_err(|_| format!("Invalid jobs count: {raw} (expected positive integer)"))?;
    if parsed == 0 {
        return Err("Invalid jobs count: 0 (expected positive integer)".to_string());
    }
    Ok(parsed)
}

pub fn run_cli(root: &Path, action: Action, args: &[String]) -> i32 {
    let mut target: Option<String> = None;
    let mut profile = Profile::Release;
    let mut jobs: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--target" => match crate::next_flag_value(args, &mut i, flag) {
                Ok(val) => target = Some(val.to_string()),
                Err(err) => {
                    eprintln!("{err}");
                    return 2;
                }
            },
            "--profile" => match crate::next_flag_value(args, &mut i, flag) {
                Ok("dev") => profile = Profile::Dev,
                Ok("release") => profile = Profile::Release,
                Ok(other) => {
                    eprintln!("Invalid profile: {other} (expected dev or release)");
                    return 2;
                }
                Err(err) => {
                    eprintln!("{err}");
                    return 2;
                }
            },
            "--jobs" | "-j" => match crate::next_flag_value(args, &mut i, flag) {
                Ok(val) => match parse_jobs_arg(val) {
                    Ok(n) => jobs = Some(n),
                    Err(err) => {
                        eprintln!("{err}");
                        return 2;
                    }
                },
                Err(err) => {
                    eprintln!("{err}");
                    return 2;
                }
            },
            "-h" | "--help" => {
                usage();
                return 0;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                usage();
                return 2;
            }
        }
    }

    let resolved_jobs = jobs.or_else(|| (action == Action::Build).then(default_build_jobs));
    if let Err(err) =
        execute_with_jobs(root, action, target.as_deref(), profile, resolved_jobs, &[])
    {
        eprintln!("{err}");
        return 1;
    }
    0
}

pub fn run_macos_cross(root: &Path, args: &[String]) -> i32 {
    let Some(target) = args.first().map(String::as_str) else {
        eprintln!("Usage: macos-cross <aarch64-apple-darwin|x86_64-apple-darwin>");
        return 2;
    };
    let arch = match target {
        "aarch64-apple-darwin" => "arm64",
        "x86_64-apple-darwin" => "x86_64",
        _ => {
            eprintln!("macOS target must be aarch64-apple-darwin or x86_64-apple-darwin");
            return 1;
        }
    };
    let tools_dir = Path::new("/opt/osxcross/target/bin");
    let suffix = "-clang";
    let prefix_match = format!("{arch}-apple-darwin");
    let matches: Vec<PathBuf> = fs::read_dir(tools_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix_match) && n.ends_with(suffix))
        })
        .collect();
    if matches.len() != 1 {
        eprintln!("Expected one OSXCross compiler wrapper for {arch}");
        return 1;
    }
    let cc = matches[0].to_string_lossy().into_owned();
    let prefix = cc.strip_suffix("clang").unwrap().to_string();
    let key = target.replace('-', "_");
    let key_upper = key.to_ascii_uppercase();

    let linker_var = format!("CARGO_TARGET_{key_upper}_LINKER");
    let cc_var = format!("CC_{key}");
    let cxx_var = format!("CXX_{key}");
    let ar_var = format!("AR_{key}");
    let cxx = format!("{prefix}clang++");
    let ar = format!("{prefix}ar");

    let extra_env = [
        (linker_var.as_str(), cc.as_str()),
        (cc_var.as_str(), cc.as_str()),
        (cxx_var.as_str(), cxx.as_str()),
        (ar_var.as_str(), ar.as_str()),
    ];
    if let Err(err) = execute(
        root,
        Action::Package,
        Some(target),
        Profile::Release,
        &extra_env,
    ) {
        eprintln!("{err}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_for_cpus_subtracts_two_and_clamps_to_one() {
        assert_eq!(jobs_for_cpus(0), 1);
        assert_eq!(jobs_for_cpus(1), 1);
        assert_eq!(jobs_for_cpus(2), 1);
        assert_eq!(jobs_for_cpus(3), 1);
        assert_eq!(jobs_for_cpus(4), 2);
        assert_eq!(jobs_for_cpus(16), 14);
    }

    #[test]
    fn parse_jobs_arg_rejects_zero_and_invalid_values() {
        assert_eq!(parse_jobs_arg("1"), Ok(1));
        assert_eq!(parse_jobs_arg("14"), Ok(14));
        assert!(parse_jobs_arg("0").is_err());
        assert!(parse_jobs_arg("-2").is_err());
        assert!(parse_jobs_arg("abc").is_err());
    }
}
