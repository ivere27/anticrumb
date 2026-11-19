use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use xtask::publish::{
    archive_members, discover, discover_filtered, ensure_pages, preflight, prepare,
    prepare_filtered, publish, publish_ci, run_ci_with_runner, run_with_runner, sha256_file,
    target_extension, validate_archive, verify_uploads, version_from_cargo, CiChannel,
    ProcessOutput, PublishError, Runner, REPOSITORY, TARGETS,
};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

struct TarEntrySpec<'a> {
    name: &'a str,
    data: &'a [u8],
    mode: u32,
    link: Option<&'a str>,
}

fn write_tar_package(path: &Path, target: &str, entries: Option<&[TarEntrySpec<'_>]>) {
    let root = format!("anticrumb-{target}");
    let default_exe = format!("{root}/anticrumb");
    let default_lic = format!("{root}/LICENSE");
    let default_doc = format!("{root}/docs/usage.md");
    let defaults = [
        TarEntrySpec {
            name: &default_exe,
            data: b"synthetic executable",
            mode: 0o755,
            link: None,
        },
        TarEntrySpec {
            name: &default_lic,
            data: b"synthetic license",
            mode: 0o644,
            link: None,
        },
        TarEntrySpec {
            name: &default_doc,
            data: b"synthetic documentation",
            mode: 0o644,
            link: None,
        },
    ];
    let entries = entries.unwrap_or(&defaults);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = File::create(path).unwrap();
    let gz = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(gz);
    for spec in entries {
        let mut header = tar::Header::new_old();
        let name_bytes = spec.name.as_bytes();
        assert!(name_bytes.len() <= 100);
        header.as_old_mut().name = [0; 100];
        header.as_old_mut().name[..name_bytes.len()].copy_from_slice(name_bytes);
        header.set_mode(spec.mode);
        if let Some(target_link) = spec.link {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            let link_bytes = target_link.as_bytes();
            assert!(link_bytes.len() <= 100);
            header.as_old_mut().linkname = [0; 100];
            header.as_old_mut().linkname[..link_bytes.len()].copy_from_slice(link_bytes);
            header.set_cksum();
            builder.append(&header, std::io::empty()).unwrap();
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(spec.data.len() as u64);
            header.set_cksum();
            builder.append(&header, spec.data).unwrap();
        }
    }
    let gz = builder.into_inner().unwrap();
    gz.finish().unwrap();
}

struct ZipEntrySpec<'a> {
    name: &'a str,
    data: &'a [u8],
    mode: u32,
}

fn write_zip_package(path: &Path, target: &str, entries: Option<&[ZipEntrySpec<'_>]>) {
    let root = format!("anticrumb-{target}");
    let default_exe = format!("{root}/anticrumb.exe");
    let default_lic = format!("{root}/LICENSE");
    let defaults = [
        ZipEntrySpec {
            name: &default_exe,
            data: b"synthetic executable",
            mode: 0o100755,
        },
        ZipEntrySpec {
            name: &default_lic,
            data: b"synthetic license",
            mode: 0o100644,
        },
    ];
    let entries = entries.unwrap_or(&defaults);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = File::create(path).unwrap();
    let mut writer = ZipWriter::new(file);
    for spec in entries {
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(spec.mode);
        writer.start_file(spec.name, options).unwrap();
        writer.write_all(spec.data).unwrap();
    }
    writer.finish().unwrap();
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    dist: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let dist = root.join("dist");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"anticrumb\"\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            dist,
        }
    }

    fn package(&self, target: &str, folder: &str) -> PathBuf {
        let ext = target_extension(target).unwrap();
        let path = self
            .dist
            .join(folder)
            .join(format!("anticrumb-{target}{ext}"));
        if ext == ".zip" {
            write_zip_package(&path, target, None);
        } else {
            write_tar_package(&path, target, None);
        }
        path
    }
}

struct MockRunner<F>
where
    F: Fn(&[String], &Path) -> Result<ProcessOutput, PublishError>,
{
    handler: F,
}

impl<F> Runner for MockRunner<F>
where
    F: Fn(&[String], &Path) -> Result<ProcessOutput, PublishError>,
{
    fn run_process(&self, args: &[String], root: &Path) -> Result<ProcessOutput, PublishError> {
        (self.handler)(args, root)
    }
}

#[test]
fn packager_emits_one_executable_and_clears_stale_shared_libraries() {
    let f = Fixture::new();
    fs::write(f.root.join("README.md"), "Fixture readme").unwrap();
    fs::write(f.root.join("LICENSE"), "Fixture license").unwrap();
    fs::create_dir(f.root.join("docs")).unwrap();
    fs::write(f.root.join("docs/usage.md"), "Fixture documentation").unwrap();

    let fakebin = f.root.join("fakebin");
    fs::create_dir(&fakebin).unwrap();
    let cargo_log = f.root.join("cargo-args.log");
    let cargo_bin = fakebin.join("cargo");
    let cargo_content = if cfg!(windows) {
        "@echo off\r\necho %* >> \"%ANTICRUMB_CARGO_LOG%\"\r\n"
    } else {
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ANTICRUMB_CARGO_LOG\"\n"
    };
    let cargo_path = if cfg!(windows) {
        fakebin.join("cargo.cmd")
    } else {
        cargo_bin.clone()
    };
    fs::write(&cargo_path, cargo_content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cargo_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let xtask_bin = PathBuf::from(env!("CARGO_BIN_EXE_xtask"));
    let _ = workspace_root();
    for (target, executable, library) in [
        ("x86_64-unknown-linux-gnu", "anticrumb", "libanticrumb.so"),
        ("aarch64-apple-darwin", "anticrumb", "libanticrumb.dylib"),
        ("x86_64-pc-windows-msvc", "anticrumb.exe", "anticrumb.dll"),
    ] {
        let _ = fs::remove_file(&cargo_log);
        let rustc_path = if cfg!(windows) {
            fakebin.join("rustc.cmd")
        } else {
            fakebin.join("rustc")
        };
        let rustc_content = if cfg!(windows) {
            format!("@echo off\r\necho host: {target}\r\n")
        } else {
            format!("#!/bin/sh\nprintf 'host: {target}\\n'\n")
        };
        fs::write(&rustc_path, rustc_content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&rustc_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let binaries = f.root.join("target").join(target).join("release");
        fs::create_dir_all(&binaries).unwrap();
        fs::write(binaries.join(executable), b"fixture executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(binaries.join(executable), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        fs::write(binaries.join(library), b"stale build output").unwrap();
        let destination = f.dist.join(format!("anticrumb-{target}"));
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join(library), b"stale package output").unwrap();

        let path_env = format!(
            "{}{}{}",
            fakebin.display(),
            if cfg!(windows) { ";" } else { ":" },
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(&xtask_bin)
            .args(["package", "--target", target])
            .env("PATH", path_env)
            .env("CARGO", &cargo_path)
            .env("RUSTC", &rustc_path)
            .env("ANTICRUMB_ROOT", &f.root)
            .env("ANTICRUMB_CARGO_LOG", &cargo_log)
            .env("CARGO_TARGET_DIR", f.root.join("target"))
            .output()
            .expect("run xtask package");
        assert!(
            out.status.success(),
            "xtask package failed for {target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let logged = fs::read_to_string(&cargo_log).unwrap();
        assert!(!logged.split_whitespace().any(|tok| tok == "--lib"));
        assert!(!destination.join(library).exists());

        let ext = target_extension(target).unwrap();
        let archive = f.dist.join(format!("anticrumb-{target}{ext}"));
        validate_archive(&archive, target).unwrap();
        let files: BTreeSet<String> = archive_members(&archive)
            .unwrap()
            .into_iter()
            .filter(|m| !m.directory)
            .map(|m| m.name.split_once('/').unwrap().1.to_string())
            .collect();
        let expected: BTreeSet<String> = [executable, "README.md", "LICENSE", "docs/usage.md"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(files, expected);
    }
}

#[test]
fn publisher_rejects_stale_shared_library_packages_on_every_platform() {
    let f = Fixture::new();
    for (target, library) in [
        ("x86_64-unknown-linux-gnu", "libanticrumb.so"),
        ("aarch64-apple-darwin", "libanticrumb.dylib"),
        ("x86_64-pc-windows-msvc", "anticrumb.dll"),
    ] {
        let ext = target_extension(target).unwrap();
        let archive = f.dist.join(format!("anticrumb-{target}{ext}"));
        let root = format!("anticrumb-{target}");
        if ext == ".zip" {
            let exe = format!("{root}/anticrumb.exe");
            let lic = format!("{root}/LICENSE");
            let lib = format!("{root}/{library}");
            write_zip_package(
                &archive,
                target,
                Some(&[
                    ZipEntrySpec {
                        name: &exe,
                        data: b"binary",
                        mode: 0o100755,
                    },
                    ZipEntrySpec {
                        name: &lic,
                        data: b"license",
                        mode: 0o100644,
                    },
                    ZipEntrySpec {
                        name: &lib,
                        data: b"stale library",
                        mode: 0o100644,
                    },
                ]),
            );
        } else {
            let exe = format!("{root}/anticrumb");
            let lic = format!("{root}/LICENSE");
            let lib = format!("{root}/{library}");
            write_tar_package(
                &archive,
                target,
                Some(&[
                    TarEntrySpec {
                        name: &exe,
                        data: b"binary",
                        mode: 0o755,
                        link: None,
                    },
                    TarEntrySpec {
                        name: &lic,
                        data: b"license",
                        mode: 0o644,
                        link: None,
                    },
                    TarEntrySpec {
                        name: &lib,
                        data: b"stale library",
                        mode: 0o644,
                        link: None,
                    },
                ]),
            );
        }
        let err = validate_archive(&archive, target).unwrap_err();
        assert!(
            err.to_string().contains("unexpected package member"),
            "unexpected error for {target}: {err}"
        );
    }
}

fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut map = BTreeMap::new();
    fn visit(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(base, &path, out);
            } else if path.is_file() {
                out.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    if root.exists() {
        visit(root, root, &mut map);
    }
    map
}

#[test]
fn recursive_packages_produce_deterministic_archive_only_plan() {
    let f = Fixture::new();
    for (target, _) in TARGETS {
        f.package(target, &format!("docker-{target}"));
    }
    let before = snapshot_files(&f.dist);
    let (plan, uploads) = prepare(&f.root, &f.dist).unwrap();
    let expected: BTreeMap<String, PathBuf> = uploads
        .iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                p.clone(),
            )
        })
        .collect();
    assert_eq!(plan.version, "1.2.3");
    assert_eq!(plan.tag, "v1.2.3");
    let plan_targets: BTreeSet<&str> = plan.assets.keys().map(String::as_str).collect();
    let all_targets: BTreeSet<&str> = TARGETS.iter().map(|(t, _)| *t).collect();
    assert_eq!(plan_targets, all_targets);
    assert_eq!(uploads.len(), TARGETS.len());
    for (target, asset) in &plan.assets {
        let archive = &expected[&asset.name];
        assert_eq!(asset.sha256, sha256_file(archive).unwrap());
        assert_eq!(asset.size, fs::metadata(archive).unwrap().len());
        assert_eq!(
            archive.file_name().unwrap().to_string_lossy(),
            format!("anticrumb-{target}{}", target_extension(target).unwrap())
        );
    }
    let (again, repeated) = prepare(&f.root, &f.dist).unwrap();
    assert_eq!(again, plan);
    assert_eq!(repeated, uploads);
    assert_eq!(snapshot_files(&f.dist), before);
}

#[test]
fn dry_run_does_not_write_files_or_run_any_subprocess() {
    let f = Fixture::new();
    let archive = f.package("x86_64-unknown-linux-gnu", "docker-linux");
    let before = fs::read(&archive).unwrap();
    let runner = MockRunner {
        handler: |args, _| panic!("dry run invoked an external command: {args:?}"),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let args = vec![
        "--dry-run".to_string(),
        "--dist".to_string(),
        f.dist.to_string_lossy().into_owned(),
    ];
    let code = run_with_runner(&runner, &f.root, &args, &mut out, &mut err);
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
    assert_eq!(fs::read(&archive).unwrap(), before);
    let files: Vec<PathBuf> = snapshot_files(&f.dist).into_keys().collect();
    assert_eq!(
        files,
        vec![archive.strip_prefix(&f.dist).unwrap().to_path_buf()]
    );
    let stdout = String::from_utf8(out).unwrap();
    assert!(stdout.contains("--draft"));
    assert!(stdout.contains("--draft=false"));
    assert!(stdout.contains("no GitHub requests"));
    assert!(stdout.contains("GitHub SHA-256 digest"));
}

#[test]
fn duplicate_targets_and_wrong_extensions_are_rejected() {
    let f = Fixture::new();
    f.package("x86_64-unknown-linux-gnu", "docker-linux");
    f.package("x86_64-unknown-linux-gnu", "another-build");
    let err = discover(&f.dist).unwrap_err();
    assert!(err.to_string().contains("duplicate package"), "{err}");

    let f2 = Fixture::new();
    f2.package("x86_64-unknown-linux-gnu", "docker-linux");
    fs::write(
        f2.dist.join("anticrumb-aarch64-apple-darwin.zip"),
        b"not a supported archive format for this target",
    )
    .unwrap();
    let err2 = discover(&f2.dist).unwrap_err();
    assert!(err2.to_string().contains("extension"), "{err2}");
}

#[test]
fn tar_rejects_unsafe_members_and_missing_required_files_without_extracting() {
    let f = Fixture::new();
    let target = "x86_64-unknown-linux-gnu";
    let path = f.dist.join(format!("anticrumb-{target}.tar.gz"));
    let base = format!("anticrumb-{target}");
    let exe = format!("{base}/anticrumb");
    let lic = format!("{base}/LICENSE");

    let traversal = format!("{base}/../outside");
    let backslash = format!("{base}\\outside");
    let symlink = format!("{base}/docs/link");
    let env_file = format!("{base}/.env");
    let setuid = format!("{base}/docs/setuid");

    let bad_members = [
        TarEntrySpec {
            name: &traversal,
            data: b"payload",
            mode: 0o644,
            link: None,
        },
        TarEntrySpec {
            name: "/outside",
            data: b"payload",
            mode: 0o644,
            link: None,
        },
        TarEntrySpec {
            name: &backslash,
            data: b"payload",
            mode: 0o644,
            link: None,
        },
        TarEntrySpec {
            name: &symlink,
            data: b"",
            mode: 0o777,
            link: Some("../../outside"),
        },
        TarEntrySpec {
            name: &lic,
            data: b"duplicate",
            mode: 0o644,
            link: None,
        },
        TarEntrySpec {
            name: &env_file,
            data: b"private data",
            mode: 0o600,
            link: None,
        },
        TarEntrySpec {
            name: &setuid,
            data: b"payload",
            mode: 0o4755,
            link: None,
        },
    ];
    for bad in bad_members {
        write_tar_package(
            &path,
            target,
            Some(&[
                TarEntrySpec {
                    name: &exe,
                    data: b"executable",
                    mode: 0o755,
                    link: None,
                },
                TarEntrySpec {
                    name: &lic,
                    data: b"license",
                    mode: 0o644,
                    link: None,
                },
                bad,
            ]),
        );
        assert!(validate_archive(&path, target).is_err());
        assert!(!f.root.join("outside").exists());
    }

    let incomplete_sets: Vec<Vec<TarEntrySpec<'_>>> = vec![
        vec![TarEntrySpec {
            name: &exe,
            data: b"executable",
            mode: 0o755,
            link: None,
        }],
        vec![TarEntrySpec {
            name: &lic,
            data: b"license",
            mode: 0o644,
            link: None,
        }],
        vec![
            TarEntrySpec {
                name: &exe,
                data: b"binary",
                mode: 0o644,
                link: None,
            },
            TarEntrySpec {
                name: &lic,
                data: b"license",
                mode: 0o644,
                link: None,
            },
        ],
    ];
    for set in incomplete_sets {
        write_tar_package(&path, target, Some(&set));
        assert!(validate_archive(&path, target).is_err());
    }
}

#[test]
fn zip_rejects_links_traversal_and_corrupt_member_bytes() {
    let f = Fixture::new();
    let target = "x86_64-pc-windows-msvc";
    let path = f.dist.join(format!("anticrumb-{target}.zip"));
    let base = format!("anticrumb-{target}");

    for (name, mode) in [
        (format!("{base}/../outside"), 0o100644),
        (format!("{base}/anticrumb.exe"), 0o120644),
        (format!("{base}/docs/C:outside"), 0o100644),
    ] {
        write_zip_package(
            &path,
            target,
            Some(&[ZipEntrySpec {
                name: &name,
                data: b"payload",
                mode,
            }]),
        );
        assert!(
            validate_archive(&path, target).is_err(),
            "{name} should fail"
        );
    }

    write_zip_package(&path, target, None);
    let mut data = fs::read(&path).unwrap();
    let needle = b"synthetic executable";
    let pos = data
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("stored entry payload");
    data[pos..pos + needle.len()].copy_from_slice(b"corrupted executable");
    fs::write(&path, data).unwrap();
    let err = validate_archive(&path, target).unwrap_err();
    assert!(
        err.to_string().contains("checksum") || err.to_string().contains("CRC"),
        "{err}"
    );
}

#[test]
fn invalid_archives_and_nonstable_versions_fail_locally() {
    let f = Fixture::new();
    let path = f.package("x86_64-unknown-linux-gnu", "docker-linux");
    fs::write(&path, b"not a gzip archive").unwrap();
    assert!(prepare(&f.root, &f.dist).is_err());

    for version in ["1.2.3-beta.1", "01.2.3", "1.2", "v1.2.3"] {
        fs::write(
            f.root.join("Cargo.toml"),
            format!("[package]\nversion = \"{version}\"\n"),
        )
        .unwrap();
        assert!(version_from_cargo(&f.root).is_err());
    }
}

#[test]
fn tar_rejects_corrupt_gzip_footer() {
    let f = Fixture::new();
    let path = f.package("x86_64-unknown-linux-gnu", "docker-linux");
    let mut data = fs::read(&path).unwrap();
    let len = data.len();
    data[len - 8] ^= 1;
    fs::write(&path, data).unwrap();
    assert!(validate_archive(&path, "x86_64-unknown-linux-gnu").is_err());
}

#[test]
fn publication_waits_for_verified_uploads_and_uses_snapshots() {
    let f = Fixture::new();
    f.package("x86_64-unknown-linux-gnu", "docker-linux");
    let (plan, uploads) = prepare(&f.root, &f.dist).unwrap();
    let events: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let expected_sha = sha256_file(&uploads[0]).unwrap();
    let expected_size = fs::metadata(&uploads[0]).unwrap().len();
    let asset_name = uploads[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["git", "status", "--porcelain", "--untracked-files=no"]
                | ["git", "status", "--porcelain"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
                ["git", "branch", "--show-current"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: "master\n".into(),
                    stderr: String::new(),
                }),
                ["git", "rev-parse", "HEAD"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: "a".repeat(40),
                    stderr: String::new(),
                }),
                ["gh", "api", "--hostname", "github.com", p]
                    if *p == format!("repos/{REPOSITORY}") =>
                {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: json!({"permissions": {"push": true}}).to_string(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", p] if p.ends_with("heads/master") => {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: json!({"object": {"sha": "a".repeat(40)}}).to_string(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", p] if p.contains("matching-refs") => {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: "[]".into(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"]
                    if p.contains("/releases") =>
                {
                    if events.borrow().iter().any(|e| e == "create-draft") {
                        events.borrow_mut().push("verify".into());
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: json!([[{
                                "tag_name": "v1.2.3",
                                "draft": true,
                                "prerelease": false,
                                "assets": [{
                                    "name": asset_name,
                                    "size": expected_size,
                                    "state": "uploaded",
                                    "digest": format!("sha256:{expected_sha}")
                                }]
                            }]])
                            .to_string(),
                            stderr: String::new(),
                        })
                    } else {
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: "[[]]".into(),
                            stderr: String::new(),
                        })
                    }
                }
                ["gh", "api", "--hostname", "github.com", "--include", "--method", "GET", p]
                    if p.ends_with("/pages") =>
                {
                    events.borrow_mut().push("pages".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: "HTTP/2.0 200 OK\n\n{\"build_type\":\"legacy\",\"source\":{\"branch\":\"master\",\"path\":\"/\"}}\n".into(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "create", ..] => {
                    events.borrow_mut().push("create-draft".into());
                    assert!(strs.contains(&"--draft"));
                    let staged: Vec<PathBuf> = strs
                        .iter()
                        .filter(|s| s.ends_with(".tar.gz") || s.ends_with(".zip"))
                        .map(PathBuf::from)
                        .collect();
                    assert_eq!(staged.len(), uploads.len());
                    for p in staged {
                        assert!(p.is_file());
                        assert!(!p.starts_with(&f.dist));
                    }
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "edit", ..] => {
                    events.borrow_mut().push("publish".into());
                    assert!(strs.contains(&"--draft=false"));
                    assert_eq!(
                        *events.borrow(),
                        vec!["pages", "create-draft", "verify", "publish"]
                    );
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                _ => panic!("unexpected subprocess: {args:?}"),
            }
        },
    };

    let mut out = Vec::new();
    publish(&runner, &f.root, &plan, &uploads, &mut out).unwrap();
    assert_eq!(
        *events.borrow(),
        vec!["pages", "create-draft", "verify", "publish"]
    );
}

#[test]
fn incomplete_or_mismatched_uploads_leave_release_unpublished() {
    let f = Fixture::new();
    f.package("x86_64-unknown-linux-gnu", "docker-linux");
    let (plan, uploads) = prepare(&f.root, &f.dist).unwrap();
    let path = &uploads[0];
    let size = fs::metadata(path).unwrap().len();
    let digest = format!("sha256:{}", sha256_file(path).unwrap());
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let valid_asset = json!({"name": name, "size": size, "state": "uploaded", "digest": digest});
    let draft =
        json!({"tag_name": plan.tag, "draft": true, "prerelease": false, "assets": [valid_asset]});

    let ok_runner = MockRunner {
        handler: |args, _| {
            if args.iter().any(|a| a.contains("/releases")) {
                Ok(ProcessOutput {
                    status: 0,
                    stdout: json!([[draft]]).to_string(),
                    stderr: String::new(),
                })
            } else {
                panic!("unexpected: {args:?}");
            }
        },
    };
    verify_uploads(&ok_runner, &f.root, &plan.tag, &uploads).unwrap();

    let mut missing_digest = valid_asset.as_object().unwrap().clone();
    missing_digest.remove("digest");
    let invalid: Vec<(Value, &str)> = vec![
        (Value::Object(missing_digest), "SHA-256 digest"),
        (
            json!({"name": name, "size": size, "state": "uploaded", "digest": Value::Null}),
            "SHA-256 digest",
        ),
        (
            json!({"name": name, "size": size, "state": "uploaded", "digest": ""}),
            "SHA-256 digest",
        ),
        (
            json!({"name": name, "size": size, "state": "uploaded", "digest": format!("sha512:{}", "0".repeat(128))}),
            "SHA-256 digest",
        ),
        (
            json!({"name": name, "size": size, "state": "uploaded", "digest": format!("sha256:{}", "0".repeat(63))}),
            "SHA-256 digest",
        ),
        (
            json!({"name": name, "size": size, "state": "uploaded", "digest": format!("sha256:{}", "0".repeat(64))}),
            "checksum",
        ),
        (
            json!({"name": name, "size": size, "state": "starter", "digest": digest}),
            "incomplete",
        ),
        (
            json!({"name": name, "size": size + 1, "state": "uploaded", "digest": digest}),
            "incomplete",
        ),
        (
            json!({"name": "unexpected.tar.gz", "size": size, "state": "uploaded", "digest": digest}),
            "complete package set",
        ),
    ];

    for (asset, message) in invalid {
        let commands: RefCell<Vec<Vec<String>>> = RefCell::new(Vec::new());
        let bad_runner = MockRunner {
            handler: |args, _| {
                let strs: Vec<&str> = args.iter().map(String::as_str).collect();
                match strs.as_slice() {
                    ["git", "status", "--porcelain", "--untracked-files=no"]
                    | ["git", "status", "--porcelain"] => Ok(ProcessOutput { status: 0, stdout: String::new(), stderr: String::new() }),
                    ["git", "branch", "--show-current"] => Ok(ProcessOutput { status: 0, stdout: "master".into(), stderr: String::new() }),
                    ["git", "rev-parse", "HEAD"] => Ok(ProcessOutput { status: 0, stdout: "a".repeat(40), stderr: String::new() }),
                    ["gh", "api", "--hostname", "github.com", p] if *p == format!("repos/{REPOSITORY}") => Ok(ProcessOutput { status: 0, stdout: "{\"permissions\":{\"push\":true}}".into(), stderr: String::new() }),
                    ["gh", "api", "--hostname", "github.com", p] if p.ends_with("heads/master") => Ok(ProcessOutput { status: 0, stdout: json!({"object":{"sha":"a".repeat(40)}}).to_string(), stderr: String::new() }),
                    ["gh", "api", "--hostname", "github.com", p] if p.contains("matching-refs") => Ok(ProcessOutput { status: 0, stdout: "[]".into(), stderr: String::new() }),
                    ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"] if p.contains("/releases") => {
                        let out = if commands.borrow().is_empty() {
                            "[[]]".to_string()
                        } else {
                            json!([[{
                                "tag_name": plan.tag,
                                "draft": true,
                                "prerelease": false,
                                "assets": [asset]
                            }]])
                            .to_string()
                        };
                        Ok(ProcessOutput { status: 0, stdout: out, stderr: String::new() })
                    }
                    ["gh", "api", "--hostname", "github.com", "--include", "--method", "GET", p] if p.ends_with("/pages") => Ok(ProcessOutput {
                        status: 0,
                        stdout: "HTTP/2.0 200 OK\n\n{\"build_type\":\"legacy\",\"source\":{\"branch\":\"master\",\"path\":\"/\"}}\n".into(),
                        stderr: String::new(),
                    }),
                    ["gh", "release", ..] => {
                        commands.borrow_mut().push(args.to_vec());
                        Ok(ProcessOutput { status: 0, stdout: String::new(), stderr: String::new() })
                    }
                    _ => panic!("unexpected: {args:?}"),
                }
            },
        };
        let mut out = Vec::new();
        let err = publish(&bad_runner, &f.root, &plan, &uploads, &mut out).unwrap_err();
        assert!(
            err.to_string().contains(message),
            "expected {message} in {err}"
        );
        let cmds = commands.borrow();
        assert_eq!(cmds.len(), 1);
        assert_eq!(&cmds[0][..3], ["gh", "release", "create"]);
    }
}

#[test]
fn existing_release_or_mismatched_tag_cannot_be_overwritten() {
    let f = Fixture::new();
    f.package("x86_64-unknown-linux-gnu", "docker-linux");
    let (plan, _) = prepare(&f.root, &f.dist).unwrap();

    let existing_runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["git", "status", "--porcelain", "--untracked-files=no"]
                | ["git", "status", "--porcelain"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
                ["git", "branch", "--show-current"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: "master".into(),
                    stderr: String::new(),
                }),
                ["git", "rev-parse", "HEAD"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: "a".repeat(40),
                    stderr: String::new(),
                }),
                ["gh", "api", "--hostname", "github.com", p]
                    if *p == format!("repos/{REPOSITORY}") =>
                {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: "{\"permissions\":{\"push\":true}}".into(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", p] if p.ends_with("heads/master") => {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: json!({"object":{"sha":"a".repeat(40)}}).to_string(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"]
                    if p.contains("/releases") =>
                {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: json!([[{"tag_name": "v1.2.3", "draft": true}]]).to_string(),
                        stderr: String::new(),
                    })
                }
                _ => panic!("unexpected: {args:?}"),
            }
        },
    };
    let err = preflight(&existing_runner, &f.root, &plan).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    let bad_tag_runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["git", "status", "--porcelain", "--untracked-files=no"]
                | ["git", "status", "--porcelain"] => Ok(ProcessOutput { status: 0, stdout: String::new(), stderr: String::new() }),
                ["git", "branch", "--show-current"] => Ok(ProcessOutput { status: 0, stdout: "master".into(), stderr: String::new() }),
                ["git", "rev-parse", "HEAD"] => Ok(ProcessOutput { status: 0, stdout: "a".repeat(40), stderr: String::new() }),
                ["gh", "api", "--hostname", "github.com", p] if *p == format!("repos/{REPOSITORY}") => Ok(ProcessOutput { status: 0, stdout: "{\"permissions\":{\"push\":true}}".into(), stderr: String::new() }),
                ["gh", "api", "--hostname", "github.com", p] if p.ends_with("heads/master") => Ok(ProcessOutput { status: 0, stdout: json!({"object":{"sha":"a".repeat(40)}}).to_string(), stderr: String::new() }),
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"] if p.contains("/releases") => Ok(ProcessOutput { status: 0, stdout: "[[]]".into(), stderr: String::new() }),
                ["gh", "api", "--hostname", "github.com", p] if p.contains("matching-refs") => Ok(ProcessOutput {
                    status: 0,
                    stdout: json!([{"ref": "refs/tags/v1.2.3", "object": {"type": "commit", "sha": "b".repeat(40)}}]).to_string(),
                    stderr: String::new(),
                }),
                _ => panic!("unexpected: {args:?}"),
            }
        },
    };
    let err2 = preflight(&bad_tag_runner, &f.root, &plan).unwrap_err();
    assert!(err2.to_string().contains("does not point"), "{err2}");
}

#[test]
fn preflight_checks_git_status_ignoring_untracked_files() {
    let f = Fixture::new();
    f.package("x86_64-unknown-linux-gnu", "docker-linux");
    let (plan, _) = prepare(&f.root, &f.dist).unwrap();

    let dirty_runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["git", "status", "--porcelain", "--untracked-files=no"] => Ok(ProcessOutput {
                    status: 0,
                    stdout: " M src/lib.rs\n".into(),
                    stderr: String::new(),
                }),
                _ => panic!("unexpected: {args:?}"),
            }
        },
    };
    let err = preflight(&dirty_runner, &f.root, &plan).unwrap_err();
    assert!(
        err.to_string()
            .contains("the Git working tree must be clean"),
        "{err}"
    );
}

fn http_response(status: u16, value: Value, returncode: i32) -> ProcessOutput {
    ProcessOutput {
        status: returncode,
        stdout: format!("HTTP/2.0 {status} status\nContent-Type: application/json\n\n{value}\n"),
        stderr: if returncode == 0 {
            String::new()
        } else {
            format!("gh: failure (HTTP {status})")
        },
    }
}

#[test]
fn pages_optional_build_type_preserves_verified_branch_root_configuration() {
    let f = Fixture::new();
    for optional in [
        json!({"source": {"branch": "master", "path": "/"}}),
        json!({"build_type": Value::Null, "source": {"branch": "master", "path": "/"}}),
    ] {
        let calls = RefCell::new(Vec::new());
        let r1 = MockRunner {
            handler: |args, _| {
                calls.borrow_mut().push(args[6].clone());
                Ok(http_response(200, optional.clone(), 0))
            },
        };
        let mut out = Vec::new();
        ensure_pages(&r1, &f.root, &mut out).unwrap();
        assert_eq!(*calls.borrow(), vec!["GET"]);

        let calls2 = RefCell::new(Vec::new());
        let r2 = MockRunner {
            handler: |args, _| {
                calls2.borrow_mut().push(args[6].clone());
                if args[6] == "GET" {
                    Ok(http_response(404, json!({"message": "Not Found"}), 1))
                } else {
                    Ok(http_response(201, optional.clone(), 0))
                }
            },
        };
        ensure_pages(&r2, &f.root, &mut out).unwrap();
        assert_eq!(*calls2.borrow(), vec!["GET", "POST"]);
    }

    for unconfirmed in [
        json!({"build_type": "workflow", "source": {"branch": "master", "path": "/"}}),
        json!({"source": {"branch": "other", "path": "/"}}),
        json!({"build_type": Value::Null, "source": {"branch": "master", "path": "/docs"}}),
    ] {
        let calls = RefCell::new(Vec::new());
        let r = MockRunner {
            handler: |args, _| {
                calls.borrow_mut().push(args[6].clone());
                if args[6] == "PUT" {
                    Ok(http_response(204, json!({}), 0))
                } else {
                    Ok(http_response(200, unconfirmed.clone(), 0))
                }
            },
        };
        let mut out = Vec::new();
        let err = ensure_pages(&r, &f.root, &mut out).unwrap_err();
        assert!(err.to_string().contains("did not confirm Pages"), "{err}");
        assert_eq!(*calls.borrow(), vec!["GET", "PUT", "GET"]);
    }
}

#[test]
fn pages_creation_requires_an_actual_404_and_precedes_release_creation() {
    let f = Fixture::new();
    let branch_root = json!({"build_type": "legacy", "source": {"branch": "master", "path": "/"}});
    let recorded: RefCell<Vec<Vec<String>>> = RefCell::new(Vec::new());
    let r = MockRunner {
        handler: |args, _| {
            recorded.borrow_mut().push(args.to_vec());
            if args[6] == "GET" {
                Ok(http_response(404, json!({"message": "Not Found"}), 1))
            } else {
                Ok(http_response(201, branch_root.clone(), 0))
            }
        },
    };
    let mut out = Vec::new();
    ensure_pages(&r, &f.root, &mut out).unwrap();
    let calls = recorded.borrow();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].iter().any(|a| a == "GET"));
    assert!(calls[1].iter().any(|a| a == "POST"));
    assert!(calls[1].iter().any(|a| a == "build_type=legacy"));
    assert!(calls[1].iter().any(|a| a == "source[branch]=master"));
    assert!(calls[1].iter().any(|a| a == "source[path]=/"));

    for rejected in [
        http_response(403, json!({"message": "Forbidden"}), 1),
        http_response(429, json!({"message": "Rate limited"}), 1),
        ProcessOutput {
            status: 1,
            stdout: String::new(),
            stderr: "network unavailable".into(),
        },
    ] {
        let count = RefCell::new(0);
        let r_bad = MockRunner {
            handler: |_, _| {
                *count.borrow_mut() += 1;
                Ok(rejected.clone())
            },
        };
        assert!(ensure_pages(&r_bad, &f.root, &mut out).is_err());
        assert_eq!(*count.borrow(), 1);
    }

    let r_post_403 = MockRunner {
        handler: |args, _| {
            if args[6] == "GET" {
                Ok(http_response(404, json!({}), 1))
            } else {
                Ok(http_response(403, json!({}), 1))
            }
        },
    };
    let err = ensure_pages(&r_post_403, &f.root, &mut out).unwrap_err();
    assert!(
        err.to_string().contains("Pages POST failed (HTTP 403)"),
        "{err}"
    );

    let previous_cases = [
        json!({"build_type": "workflow"}),
        json!({"build_type": "legacy", "source": {"branch": "gh-pages", "path": "/docs"}}),
    ];
    for previous in previous_cases {
        let step = RefCell::new(0);
        let r_update = MockRunner {
            handler: |_, _| {
                let mut s = step.borrow_mut();
                let resp = match *s {
                    0 => http_response(200, previous.clone(), 0),
                    1 => http_response(204, json!({}), 0),
                    _ => http_response(200, branch_root.clone(), 0),
                };
                *s += 1;
                Ok(resp)
            },
        };
        ensure_pages(&r_update, &f.root, &mut out).unwrap();
        assert_eq!(*step.borrow(), 3);
    }
}

#[test]
fn docker_only_filters_out_non_docker_packages_and_succeeds() {
    let f = Fixture::new();
    let target = "x86_64-unknown-linux-gnu";
    // Simulate docker build artifact
    f.package(target, "docker-linux");
    // Simulate local test/build artifact in another folder
    f.package(target, "local");

    // Default discover sees both and fails on duplicate
    let err = discover(&f.dist).unwrap_err();
    assert!(err.to_string().contains("duplicate package"), "{err}");

    // discover_filtered with docker_only = true ignores 'local' and succeeds
    let pkgs = discover_filtered(&f.dist, true).unwrap();
    assert_eq!(pkgs.len(), 1);
    assert!(pkgs.contains_key(target));
    assert!(pkgs[target].to_string_lossy().contains("docker-linux"));

    let (plan, uploads) = prepare_filtered(&f.root, &f.dist, true).unwrap();
    assert_eq!(plan.assets.len(), 1);
    assert_eq!(uploads.len(), 1);
}

#[test]
fn docker_only_fails_when_no_docker_packages_exist() {
    let f = Fixture::new();
    let target = "x86_64-unknown-linux-gnu";
    f.package(target, "local");

    let err = discover_filtered(&f.dist, true).unwrap_err();
    assert!(
        err.to_string().contains("no docker release archives in"),
        "{err}"
    );
}

#[test]
fn cli_docker_only_dry_run_ignores_local_packages() {
    let f = Fixture::new();
    let target = "x86_64-unknown-linux-gnu";
    f.package(target, "docker-linux");
    f.package(target, "local");

    let runner = MockRunner {
        handler: |args, _| panic!("dry run invoked an external command: {args:?}"),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let args = vec![
        "--dry-run".to_string(),
        "--docker-only".to_string(),
        "--dist".to_string(),
        f.dist.to_string_lossy().into_owned(),
    ];
    let code = run_with_runner(&runner, &f.root, &args, &mut out, &mut err);
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
    let stdout = String::from_utf8(out).unwrap();
    assert!(stdout.contains("docker-linux"));
    assert!(!stdout.contains("/local/") && !stdout.contains("\\local\\"));
}

#[test]
fn release_ci_stable_validates_tag_and_publishes_verified_draft() {
    let f = Fixture::new();
    let archive = f.package("x86_64-unknown-linux-gnu", "ci-linux");
    let expected_sha = sha256_file(&archive).unwrap();
    let expected_size = fs::metadata(&archive).unwrap().len();
    let asset_name = archive.file_name().unwrap().to_string_lossy().into_owned();
    let sha = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    let events: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"]
                    if p.contains("/releases") =>
                {
                    if events.borrow().iter().any(|e| e == "create-draft") {
                        events.borrow_mut().push("verify".into());
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: json!([[{
                                "tag_name": "v1.2.3",
                                "draft": true,
                                "prerelease": false,
                                "assets": [{
                                    "name": asset_name,
                                    "size": expected_size,
                                    "state": "uploaded",
                                    "digest": format!("sha256:{expected_sha}")
                                }]
                            }]])
                            .to_string(),
                            stderr: String::new(),
                        })
                    } else {
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: "[[]]".into(),
                            stderr: String::new(),
                        })
                    }
                }
                ["gh", "release", "create", "v1.2.3", ..] => {
                    assert!(strs.contains(&"--draft"));
                    assert!(strs.contains(&sha));
                    events.borrow_mut().push("create-draft".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "edit", "v1.2.3", ..] => {
                    assert!(strs.contains(&"--draft=false"));
                    assert!(strs.contains(&"--latest"));
                    events.borrow_mut().push("publish".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                _ => panic!("unexpected subprocess: {args:?}"),
            }
        },
    };

    let mut out = Vec::new();
    let mut err = Vec::new();
    let args: Vec<String> = [
        "--channel",
        "stable",
        "--tag",
        "v1.2.3",
        "--sha",
        sha,
        "--dist",
        f.dist.to_str().unwrap(),
        "--require-target",
        "x86_64-unknown-linux-gnu",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let code = run_ci_with_runner(&runner, &f.root, &args, &mut out, &mut err);
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
    assert_eq!(*events.borrow(), vec!["create-draft", "verify", "publish"]);

    // Reject tag mismatched with Cargo.toml and missing required target before calling gh.
    let panic_runner = MockRunner {
        handler: |args, _| panic!("should not invoke gh on invalid inputs: {args:?}"),
    };
    let bad_tag_args: Vec<String> = [
        "--channel",
        "stable",
        "--tag",
        "v9.9.9",
        "--sha",
        sha,
        "--dist",
        f.dist.to_str().unwrap(),
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        run_ci_with_runner(&panic_runner, &f.root, &bad_tag_args, &mut out, &mut err),
        1
    );
    let missing_target_args: Vec<String> = [
        "--channel",
        "stable",
        "--tag",
        "v1.2.3",
        "--sha",
        sha,
        "--dist",
        f.dist.to_str().unwrap(),
        "--require-target",
        "aarch64-apple-darwin",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        run_ci_with_runner(
            &panic_runner,
            &f.root,
            &missing_target_args,
            &mut out,
            &mut err
        ),
        1
    );
}

#[test]
fn release_ci_nightly_creates_initial_prerelease_and_rolls_existing_release() {
    let f = Fixture::new();
    let archive = f.package("x86_64-unknown-linux-gnu", "ci-linux");
    let expected_sha = sha256_file(&archive).unwrap();
    let expected_size = fs::metadata(&archive).unwrap().len();
    let asset_name = archive.file_name().unwrap().to_string_lossy().into_owned();
    let sha = "abcdef1234567890abcdef1234567890abcdef12";

    // Case 1: initial nightly release creation
    let create_events: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let create_runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"]
                    if p.contains("/releases") =>
                {
                    if create_events.borrow().iter().any(|e| e == "create-draft") {
                        create_events.borrow_mut().push("verify".into());
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: json!([[{
                                "tag_name": "nightly",
                                "draft": true,
                                "prerelease": true,
                                "assets": [{
                                    "name": asset_name,
                                    "size": expected_size,
                                    "state": "uploaded",
                                    "digest": format!("sha256:{expected_sha}")
                                }]
                            }]])
                            .to_string(),
                            stderr: String::new(),
                        })
                    } else {
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: "[[]]".into(),
                            stderr: String::new(),
                        })
                    }
                }
                ["gh", "release", "create", "nightly", ..] => {
                    assert!(strs.contains(&"--draft"));
                    assert!(strs.contains(&"--prerelease"));
                    assert!(strs.contains(&"--latest=false"));
                    assert!(strs.contains(&"anticrumb nightly (abcdef1)"));
                    create_events.borrow_mut().push("create-draft".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "edit", "nightly", ..] => {
                    assert!(strs.contains(&"--draft=false"));
                    assert!(strs.contains(&"--prerelease"));
                    assert!(strs.contains(&"--latest=false"));
                    create_events.borrow_mut().push("publish".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                _ => panic!("unexpected subprocess: {args:?}"),
            }
        },
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let args: Vec<String> = [
        "--channel",
        "nightly",
        "--sha",
        sha,
        "--dist",
        f.dist.to_str().unwrap(),
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        run_ci_with_runner(&create_runner, &f.root, &args, &mut out, &mut err),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        *create_events.borrow(),
        vec!["create-draft", "verify", "publish"]
    );

    // Case 2: existing nightly release gets clobber-uploaded, stale assets deleted, verified, git ref moved, and edited
    let update_events: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let update_runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"]
                    if p.contains("/releases") =>
                {
                    if update_events.borrow().iter().any(|e| e == "upload") {
                        update_events.borrow_mut().push("verify".into());
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: json!([[{
                                "tag_name": "nightly",
                                "draft": false,
                                "prerelease": true,
                                "assets": [{
                                    "name": asset_name,
                                    "size": expected_size,
                                    "state": "uploaded",
                                    "digest": format!("sha256:{expected_sha}")
                                }]
                            }]])
                            .to_string(),
                            stderr: String::new(),
                        })
                    } else {
                        Ok(ProcessOutput {
                            status: 0,
                            stdout: json!([[{
                                "tag_name": "nightly",
                                "draft": false,
                                "prerelease": true,
                                "assets": [
                                    {"name": asset_name},
                                    {"name": "anticrumb-stale-target.tar.gz"}
                                ]
                            }]])
                            .to_string(),
                            stderr: String::new(),
                        })
                    }
                }
                ["gh", "release", "upload", "nightly", ..] => {
                    assert!(strs.contains(&"--clobber"));
                    update_events.borrow_mut().push("upload".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "delete-asset", "nightly", "anticrumb-stale-target.tar.gz", ..] =>
                {
                    assert!(strs.contains(&"--yes"));
                    update_events.borrow_mut().push("delete-stale".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", p] if p.contains("matching-refs") => {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: json!([{"ref": "refs/tags/nightly"}]).to_string(),
                        stderr: String::new(),
                    })
                }
                ["gh", "api", "--hostname", "github.com", "--method", "PATCH", p, ..]
                    if p.ends_with("/git/refs/tags/nightly") =>
                {
                    assert!(strs.contains(&&*format!("sha={sha}")));
                    update_events.borrow_mut().push("patch-ref".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "edit", "nightly", ..] => {
                    assert!(strs.contains(&"--target"));
                    assert!(strs.contains(&sha));
                    assert!(strs.contains(&"--prerelease"));
                    assert!(strs.contains(&"--latest=false"));
                    assert!(strs.contains(&"anticrumb nightly (abcdef1)"));
                    update_events.borrow_mut().push("edit".into());
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
                _ => panic!("unexpected subprocess: {args:?}"),
            }
        },
    };
    out.clear();
    err.clear();
    assert_eq!(
        run_ci_with_runner(&update_runner, &f.root, &args, &mut out, &mut err),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        *update_events.borrow(),
        vec!["upload", "delete-stale", "verify", "patch-ref", "edit"]
    );

    // Case 3: publish_ci rejects invalid short/non-hex SHA before invoking subprocesses, and reports nightly-specific verification state errors
    let (mut plan, uploads) = prepare(&f.root, &f.dist).unwrap();
    plan.tag = "nightly".to_string();
    let panic_runner = MockRunner {
        handler: |args, _| panic!("unexpected subprocess on invalid SHA: {args:?}"),
    };
    let mut dummy_out = Vec::new();
    let short_err = publish_ci(
        &panic_runner,
        &f.root,
        CiChannel::Nightly,
        &plan,
        &uploads,
        "abc",
        &mut dummy_out,
    )
    .unwrap_err();
    assert!(short_err.to_string().contains("7..=40"), "{short_err}");

    let non_prerelease_runner = MockRunner {
        handler: |args, _| {
            let strs: Vec<&str> = args.iter().map(String::as_str).collect();
            match strs.as_slice() {
                ["gh", "api", "--hostname", "github.com", p, "--paginate", "--slurp"]
                    if p.contains("/releases") =>
                {
                    Ok(ProcessOutput {
                        status: 0,
                        stdout: json!([[{
                            "tag_name": "nightly",
                            "draft": false,
                            "prerelease": false,
                            "assets": []
                        }]])
                        .to_string(),
                        stderr: String::new(),
                    })
                }
                ["gh", "release", "upload", "nightly", ..] => Ok(ProcessOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
                _ => panic!("unexpected subprocess: {args:?}"),
            }
        },
    };
    let state_err = publish_ci(
        &non_prerelease_runner,
        &f.root,
        CiChannel::Nightly,
        &plan,
        &uploads,
        sha,
        &mut dummy_out,
    )
    .unwrap_err();
    assert!(
        state_err.to_string().contains("nightly prerelease"),
        "{state_err}"
    );
}
