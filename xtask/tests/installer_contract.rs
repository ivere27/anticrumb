use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

const VERSION: &str = "0.1.0";
const BINARY: &[u8] = b"downloaded binary fixture; this must never be executed\n";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn sha256_bytes(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn release_for_tag(tag: &str, prerelease: bool, name: &str, data: &[u8]) -> Value {
    let url = format!("https://github.com/ivere27/anticrumb/releases/download/{tag}/{name}");
    json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": prerelease,
        "assets": [{
            "name": name,
            "browser_download_url": url,
            "size": data.len(),
            "digest": format!("sha256:{}", sha256_bytes(data)),
            "state": "uploaded"
        }]
    })
}

fn release_for(name: &str, data: &[u8]) -> Value {
    release_for_tag(&format!("v{VERSION}"), false, name, data)
}

fn write_windows_fixture_zip(archive: &Path, target: &str, symlink: bool) {
    let file = File::create(archive).unwrap();
    let mut writer = ZipWriter::new(file);
    let entry_name = format!("anticrumb-{target}/anticrumb.exe");
    if symlink {
        // ZipWriter::start_file strips upper file-type bits and forces S_IFREG;
        // add_symlink writes S_IFLNK (0o120000) into external attributes.
        writer
            .add_symlink(entry_name, "../../outside", SimpleFileOptions::default())
            .unwrap();
    } else {
        writer
            .start_file(
                entry_name,
                SimpleFileOptions::default().unix_permissions(0o100755),
            )
            .unwrap();
        writer.write_all(BINARY).unwrap();
    }
    writer
        .start_file("../../outside", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"evil").unwrap();
    writer.finish().unwrap();
}

#[test]
fn windows_zip_fixture_encodes_regular_and_symlink_entries() {
    let root = TempDir::new().unwrap();
    let target = "x86_64-pc-windows-msvc";
    let expected_name = format!("anticrumb-{target}/anticrumb.exe");

    let regular = root.path().join("regular.zip");
    write_windows_fixture_zip(&regular, target, false);
    let mut regular_archive = zip::ZipArchive::new(File::open(&regular).unwrap()).unwrap();
    let regular_entry = regular_archive.by_name(&expected_name).unwrap();
    assert!(!regular_entry.is_symlink());
    assert_eq!(regular_entry.unix_mode(), Some(0o100755));

    let symlink = root.path().join("symlink.zip");
    write_windows_fixture_zip(&symlink, target, true);
    let mut symlink_archive = zip::ZipArchive::new(File::open(&symlink).unwrap()).unwrap();
    let symlink_entry = symlink_archive.by_name(&expected_name).unwrap();
    assert!(symlink_entry.is_symlink());
    assert_eq!(symlink_entry.unix_mode(), Some(0o120777));
}

#[cfg(unix)]
mod posix_shell_installer {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::os::unix::fs::PermissionsExt;

    struct ShellFixture {
        root: TempDir,
        home: PathBuf,
        downloads: PathBuf,
        fakebin: PathBuf,
        requests: PathBuf,
        system: String,
        arch: String,
        status_override: Option<String>,
        version_override: Option<String>,
        release: Value,
    }

    impl ShellFixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let home = root.path().join("user");
            let downloads = root.path().join("downloads");
            let fakebin = root.path().join("commands");
            fs::create_dir(&home).unwrap();
            fs::create_dir(&downloads).unwrap();
            fs::create_dir(&fakebin).unwrap();

            let curl_script = r#"#!/bin/sh
set -eu
out=""
write_out=0
url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) out="$2"; shift 2 ;;
        --write-out) write_out=1; shift 2 ;;
        https://*) url="$1"; shift ;;
        -H|-A|--connect-timeout|--max-time) shift 2 ;;
        *) shift ;;
    esac
done
printf '%s\n' "$url" >> "$INSTALLER_TEST_REQUESTS"
case "$url" in
    https://api.github.com/*) name="release.json" ;;
    *) name="${url##*/}" ;;
esac
source="$INSTALLER_TEST_DOWNLOADS/$name"
if [ "$write_out" -eq 1 ]; then
    if [ -n "${INSTALLER_TEST_STATUS:-}" ]; then
        status="$INSTALLER_TEST_STATUS"
    elif [ -f "$source" ]; then
        status="200"
    else
        status="404"
    fi
    printf '%s' "$status"
    if [ "$status" != "200" ]; then
        printf '{}' > "$out"
        exit 0
    fi
fi
if [ ! -f "$source" ]; then
    exit 22
fi
cp "$source" "$out"
"#;
            let curl_path = fakebin.join("curl");
            fs::write(&curl_path, curl_script).unwrap();
            fs::set_permissions(&curl_path, fs::Permissions::from_mode(0o755)).unwrap();

            let uname_script = r#"#!/bin/sh
case "$1" in
    -s) printf '%s\n' "$INSTALLER_TEST_SYSTEM" ;;
    -m) printf '%s\n' "$INSTALLER_TEST_ARCH" ;;
esac
"#;
            let uname_path = fakebin.join("uname");
            fs::write(&uname_path, uname_script).unwrap();
            fs::set_permissions(&uname_path, fs::Permissions::from_mode(0o755)).unwrap();

            let requests = root.path().join("requests");
            Self {
                root,
                home,
                downloads,
                fakebin,
                requests,
                system: "Linux".into(),
                arch: "x86_64".into(),
                status_override: None,
                version_override: None,
                release: Value::Null,
            }
        }

        fn package(
            &mut self,
            system: &str,
            architecture: &str,
            bad_hash: bool,
            binary_type: Option<tar::EntryType>,
            duplicate: bool,
        ) {
            let platform = if system == "Darwin" {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            };
            let arch_prefix = if architecture == "arm64" {
                "aarch64"
            } else {
                architecture
            };
            let target = format!("{arch_prefix}-{platform}");
            let name = format!("anticrumb-{target}.tar.gz");
            let archive_path = self.downloads.join(&name);

            {
                let file = File::create(&archive_path).unwrap();
                let gz = GzEncoder::new(file, Compression::default());
                let mut builder = tar::Builder::new(gz);

                let write_entry = |builder: &mut tar::Builder<GzEncoder<File>>,
                                   entry_name: &str,
                                   etype: tar::EntryType| {
                    let mut header = tar::Header::new_old();
                    let name_bytes = entry_name.as_bytes();
                    header.as_old_mut().name = [0; 100];
                    header.as_old_mut().name[..name_bytes.len()].copy_from_slice(name_bytes);
                    header.set_mode(0o755);
                    header.set_entry_type(etype);
                    if etype == tar::EntryType::Regular {
                        header.set_size(BINARY.len() as u64);
                        header.set_cksum();
                        builder.append(&header, BINARY).unwrap();
                    } else {
                        let link_target = self.root.path().join("outside");
                        let link_str = link_target.to_string_lossy();
                        let link_bytes = link_str.as_bytes();
                        header.as_old_mut().linkname = [0; 100];
                        header.as_old_mut().linkname[..link_bytes.len()]
                            .copy_from_slice(link_bytes);
                        header.set_size(0);
                        header.set_cksum();
                        builder.append(&header, std::io::empty()).unwrap();
                    }
                };

                let entry_name = format!("anticrumb-{target}/anticrumb");
                write_entry(
                    &mut builder,
                    &entry_name,
                    binary_type.unwrap_or(tar::EntryType::Regular),
                );
                if duplicate {
                    write_entry(&mut builder, &entry_name, tar::EntryType::Regular);
                }

                let mut outside = tar::Header::new_old();
                let outside_name = b"../../outside";
                outside.as_old_mut().name = [0; 100];
                outside.as_old_mut().name[..outside_name.len()].copy_from_slice(outside_name);
                outside.set_mode(0o644);
                outside.set_entry_type(tar::EntryType::Regular);
                outside.set_size(4);
                outside.set_cksum();
                builder.append(&outside, &b"evil"[..]).unwrap();

                let gz = builder.into_inner().unwrap();
                gz.finish().unwrap();
            }

            let bytes = fs::read(&archive_path).unwrap();
            let mut release = release_for(&name, &bytes);
            if bad_hash {
                release["assets"][0]["digest"] =
                    Value::String(format!("sha256:{}", "0".repeat(64)));
            }
            fs::write(
                self.downloads.join("release.json"),
                serde_json::to_string(&release).unwrap(),
            )
            .unwrap();
            self.release = release;
            self.system = system.into();
            self.arch = architecture.into();
        }

        fn install(&self) -> Output {
            self.install_with_args(&[])
        }

        fn install_with_args(&self, args: &[&str]) -> Output {
            let mut cmd = Command::new("sh");
            cmd.arg(workspace_root().join("install.sh"));
            cmd.args(args);
            cmd.env("HOME", &self.home)
                .env("SHELL", "/bin/sh")
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.fakebin.display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env("INSTALLER_TEST_DOWNLOADS", &self.downloads)
                .env("INSTALLER_TEST_REQUESTS", &self.requests)
                .env("INSTALLER_TEST_SYSTEM", &self.system)
                .env("INSTALLER_TEST_ARCH", &self.arch)
                .env_remove("ANTICRUMB_VERSION")
                .env_remove("ANTICRUMB_INSTALL_DIR");
            if let Some(status) = &self.status_override {
                cmd.env("INSTALLER_TEST_STATUS", status);
            } else {
                cmd.env_remove("INSTALLER_TEST_STATUS");
            }
            if let Some(version) = &self.version_override {
                cmd.env("ANTICRUMB_VERSION", version);
            }
            cmd.output().unwrap()
        }
    }

    #[test]
    fn verified_install_is_user_local_and_never_extracts_other_paths() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let result = f.install();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let installed = f.home.join(".local/bin/anticrumb");
        assert_eq!(fs::read(&installed).unwrap(), BINARY);
        let mode = fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(!f.root.path().join("outside").exists());
        let first_req = fs::read_to_string(&f.requests)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            first_req,
            "https://api.github.com/repos/ivere27/anticrumb/releases/latest"
        );
        let profile = fs::read_to_string(f.home.join(".profile")).unwrap();
        assert!(profile.contains("export PATH=\"$HOME/.local/bin:$PATH\""));
        assert!(f.install().status.success());
        let profile2 = fs::read_to_string(f.home.join(".profile")).unwrap();
        assert_eq!(profile2.matches("# anticrumb").count(), 1);
    }

    #[test]
    fn macos_arm_package_detection() {
        let mut f = ShellFixture::new();
        f.package("Darwin", "arm64", false, None, false);
        let result = f.install();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(f.home.join(".local/bin/anticrumb").exists());
    }

    #[test]
    fn leftover_shared_library_does_not_block_install_or_get_modified() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let destination = f.home.join(".local/bin");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("anticrumb"), b"previous binary").unwrap();
        fs::write(destination.join("libanticrumb.so"), b"previous library").unwrap();
        let result = f.install();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read(destination.join("anticrumb")).unwrap(), BINARY);
        assert_eq!(
            fs::read(destination.join("libanticrumb.so")).unwrap(),
            b"previous library"
        );
    }

    #[test]
    fn checksum_failure_preserves_previous_install() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", true, None, false);
        let installed = f.home.join(".local/bin/anticrumb");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, b"previous version").unwrap();
        let result = f.install();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("SHA-256 mismatch"));
        assert_eq!(fs::read(&installed).unwrap(), b"previous version");
    }

    #[test]
    fn failed_atomic_replacement_preserves_previous_install() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let installed = f.home.join(".local/bin/anticrumb");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, b"previous version").unwrap();
        let mv_path = f.fakebin.join("mv");
        fs::write(&mv_path, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&mv_path, fs::Permissions::from_mode(0o755)).unwrap();
        let result = f.install();
        assert!(!result.status.success());
        assert_eq!(fs::read(&installed).unwrap(), b"previous version");
        let entries: Vec<_> = fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries, vec![installed]);
    }

    #[test]
    fn links_and_duplicate_binary_entries_are_rejected() {
        for (btype, dup) in [
            (Some(tar::EntryType::Symlink), false),
            (Some(tar::EntryType::Link), false),
            (None, true),
        ] {
            let mut f = ShellFixture::new();
            f.package("Linux", "x86_64", false, btype, dup);
            let result = f.install();
            assert!(!result.status.success());
            assert!(!f.home.join(".local/bin/anticrumb").exists());
            assert!(!f.root.path().join("outside").exists());
        }
    }

    #[test]
    fn missing_release_does_not_create_installation() {
        let f = ShellFixture::new();
        let result = f.install();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("No stable release"));
        assert!(!f.home.join(".local/bin").exists());
    }

    #[test]
    fn api_rate_limit_is_actionable_and_never_installs() {
        for status in ["403", "429"] {
            let mut f = ShellFixture::new();
            f.status_override = Some(status.into());
            let result = f.install();
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stderr).contains("rate-limited"));
            assert!(!f.home.join(".local/bin").exists());
        }
    }

    #[test]
    fn explicit_version_uses_fixed_tag_endpoint() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        f.version_override = Some(format!("v{VERSION}"));
        let result = f.install();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let first_req = fs::read_to_string(&f.requests)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            first_req,
            format!("https://api.github.com/repos/ivere27/anticrumb/releases/tags/v{VERSION}")
        );
    }

    #[test]
    fn minified_reordered_json_ignores_escaped_body_and_nested_uploader_fields() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let mut asset = f.release["assets"][0].clone();
        asset["uploader"] = json!({
            "name": asset["name"],
            "digest": "wrong",
            "size": 1,
            "body": "Quotes \" and slashes \\ and emoji 🐈"
        });
        let release = json!({
            "body": "Ignore \"]},\\ and $(do-not-execute) \n 🐈",
            "assets": [asset],
            "prerelease": false,
            "draft": false,
            "tag_name": format!("v{VERSION}")
        });
        let raw = serde_json::to_string(&release)
            .unwrap()
            .replace("\"name\":", "\"na\\u006de\":");
        fs::write(f.downloads.join("release.json"), raw).unwrap();
        let result = f.install();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::read(f.home.join(".local/bin/anticrumb")).unwrap(),
            BINARY
        );
    }

    #[test]
    fn invalid_metadata_preserves_install_before_archive_download() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let installed = f.home.join(".local/bin/anticrumb");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, b"previous version").unwrap();

        let mutations: Vec<fn(&mut Value)> = vec![
            |r| r["tag_name"] = json!("v01.2.3"),
            |r| r["tag_name"] = json!("v1.2.3-beta"),
            |r| r["draft"] = json!(true),
            |r| r["prerelease"] = json!("false"),
            |r| {
                r["assets"][0].as_object_mut().unwrap().remove("digest");
            },
            |r| r["assets"][0]["digest"] = Value::Null,
            |r| r["assets"][0]["digest"] = json!(format!("md5:{}", "a".repeat(64))),
            |r| r["assets"][0]["size"] = json!(true),
            |r| r["assets"][0]["size"] = json!(0),
            |r| r["assets"][0]["size"] = json!(536870913),
            |r| r["assets"][0]["state"] = json!("new"),
            |r| r["assets"][0]["browser_download_url"] = json!("https://example.com/package"),
            |r| {
                let dup = r["assets"][0].clone();
                r["assets"].as_array_mut().unwrap().push(dup);
            },
        ];

        for mutate in mutations {
            let mut release = f.release.clone();
            mutate(&mut release);
            fs::write(
                f.downloads.join("release.json"),
                serde_json::to_string(&release).unwrap(),
            )
            .unwrap();
            fs::write(&f.requests, "").unwrap();
            let result = f.install();
            assert!(!result.status.success(), "{release}");
            assert_eq!(fs::read(&installed).unwrap(), b"previous version");
            assert_eq!(fs::read_to_string(&f.requests).unwrap().lines().count(), 1);
        }
    }

    #[test]
    fn duplicate_fields_and_malformed_json_are_rejected() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let raw = serde_json::to_string(&f.release).unwrap();
        let payloads = [
            raw.replace(
                "\"digest\":",
                &format!("\"digest\":\"sha256:{}\",\"digest\":", "0".repeat(64)),
            ),
            raw.replace(
                "\"tag_name\":",
                "\"tag_name\":\"v9.9.9\",\"tag_n\\u0061me\":",
            ),
            raw[..raw.len() - 1].to_string(),
            format!("{raw}{{}}"),
            raw.replace("\"draft\":false", "\"draft\":false,"),
        ];
        for payload in payloads {
            fs::write(f.downloads.join("release.json"), &payload).unwrap();
            let result = f.install();
            assert!(!result.status.success(), "{payload}");
            assert!(!f.home.join(".local/bin/anticrumb").exists());
        }
    }

    #[test]
    fn package_size_mismatch_preserves_previous_install() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        f.release["assets"][0]["size"] =
            json!(f.release["assets"][0]["size"].as_u64().unwrap() + 1);
        fs::write(
            f.downloads.join("release.json"),
            serde_json::to_string(&f.release).unwrap(),
        )
        .unwrap();
        let result = f.install();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("size mismatch"));
        assert!(!f.home.join(".local/bin/anticrumb").exists());
    }

    #[test]
    fn nightly_env_and_flag_install_verified_prerelease() {
        let mut f = ShellFixture::new();
        f.package("Linux", "x86_64", false, None, false);
        let name = "anticrumb-x86_64-unknown-linux-gnu.tar.gz";
        let archive_bytes = fs::read(f.downloads.join(name)).unwrap();
        let nightly = release_for_tag("nightly", true, name, &archive_bytes);
        fs::write(
            f.downloads.join("release.json"),
            serde_json::to_string(&nightly).unwrap(),
        )
        .unwrap();

        f.version_override = Some("nightly".into());
        let result = f.install();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::read(f.home.join(".local/bin/anticrumb")).unwrap(),
            BINARY
        );
        let first_req = fs::read_to_string(&f.requests)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            first_req,
            "https://api.github.com/repos/ivere27/anticrumb/releases/tags/nightly"
        );

        f.version_override = None;
        fs::write(&f.requests, "").unwrap();
        let via_flag = f.install_with_args(&["--nightly"]);
        assert!(
            via_flag.status.success(),
            "{}",
            String::from_utf8_lossy(&via_flag.stderr)
        );
        assert_eq!(
            fs::read_to_string(&f.requests)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
            "https://api.github.com/repos/ivere27/anticrumb/releases/tags/nightly"
        );

        // Non-prerelease must be rejected on nightly channel
        let mut not_prerelease = nightly.clone();
        not_prerelease["prerelease"] = json!(false);
        fs::write(
            f.downloads.join("release.json"),
            serde_json::to_string(&not_prerelease).unwrap(),
        )
        .unwrap();
        assert!(!f.install_with_args(&["--nightly"]).status.success());
    }
}

#[cfg(windows)]
mod powershell_installer {
    use super::*;

    fn available_shells() -> Vec<&'static str> {
        ["powershell", "pwsh"]
            .into_iter()
            .filter(|shell| {
                Command::new(shell)
                    .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                    .output()
                    .is_ok_and(|o| o.status.success())
            })
            .collect()
    }

    fn ps_quote(p: &Path) -> String {
        format!("'{}'", p.to_string_lossy().replace('\'', "''"))
    }

    #[derive(Clone, Copy, Default)]
    struct PsOptions<'a> {
        bad_hash: bool,
        symlink: bool,
        version: Option<&'a str>,
        mutate: Option<&'a dyn Fn(&mut Value)>,
    }

    fn run_ps(shell: &str, root: &TempDir, target: &str, options: PsOptions<'_>) -> Output {
        let archive = root.path().join("package.zip");
        write_windows_fixture_zip(&archive, target, options.symlink);
        let archive_bytes = fs::read(&archive).unwrap();
        let mut release = if options.version == Some("nightly") {
            release_for_tag(
                "nightly",
                true,
                &format!("anticrumb-{target}.zip"),
                &archive_bytes,
            )
        } else {
            release_for(&format!("anticrumb-{target}.zip"), &archive_bytes)
        };
        if options.bad_hash {
            release["assets"][0]["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
        }
        if let Some(m) = options.mutate {
            m(&mut release);
        }
        let metadata = root.path().join("metadata.json");
        fs::write(&metadata, serde_json::to_string(&release).unwrap()).unwrap();
        let requests = root.path().join("requests");
        fs::write(&requests, "").unwrap();
        let wrapper = root.path().join("run.ps1");
        let install_ps1 = workspace_root().join("install.ps1");
        let expected_uri = match options.version {
            Some("nightly") => {
                "https://api.github.com/repos/ivere27/anticrumb/releases/tags/nightly".to_string()
            }
            Some(v) => format!(
                "https://api.github.com/repos/ivere27/anticrumb/releases/tags/v{}",
                v.strip_prefix('v').unwrap_or(v)
            ),
            None => "https://api.github.com/repos/ivere27/anticrumb/releases/latest".to_string(),
        };
        fs::write(
            &wrapper,
            format!(
                "$ErrorActionPreference = 'Stop'\n\
                function Invoke-RestMethod {{\n\
                    param($Uri, $Headers)\n\
                    Add-Content -LiteralPath {} -Value $Uri\n\
                    switch ($Uri) {{\n\
                        '{}' {{ Get-Content -Raw -LiteralPath {} | ConvertFrom-Json }}\n\
                        default {{ throw \"Unexpected metadata URL: $Uri\" }}\n\
                    }}\n\
                }}\n\
                function Invoke-WebRequest {{ param($Uri, $OutFile, [switch]$UseBasicParsing) Add-Content -LiteralPath {} -Value $Uri; Copy-Item -LiteralPath {} -Destination $OutFile }}\n\
                $previousPath = [Environment]::GetEnvironmentVariable('Path', 'User')\n\
                try {{ & {} }}\n\
                finally {{ [Environment]::SetEnvironmentVariable('Path', $previousPath, 'User') }}\n",
                ps_quote(&requests),
                expected_uri,
                ps_quote(&metadata),
                ps_quote(&requests),
                ps_quote(&archive),
                ps_quote(&install_ps1),
            ),
        )
        .unwrap();
        let mut cmd = Command::new(shell);
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&wrapper)
        .env("USERPROFILE", root.path().join("home"))
        .env("PROCESSOR_ARCHITECTURE", "AMD64")
        .env_remove("PROCESSOR_ARCHITEW6432")
        .env_remove("ANTICRUMB_INSTALL_DIR")
        .env_remove("ANTICRUMB_VERSION");
        if let Some(v) = options.version {
            cmd.env("ANTICRUMB_VERSION", v);
        }
        cmd.output().unwrap()
    }

    #[test]
    fn powershell_installer_contracts() {
        let shells = available_shells();
        assert!(
            shells.contains(&"powershell"),
            "Windows PowerShell 5.1 (powershell) must be available on Windows to verify installer contracts; found {shells:?}"
        );
        for shell in shells {
            for target in ["x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"] {
                let root = TempDir::new().unwrap();
                let out = run_ps(shell, &root, target, PsOptions::default());
                assert!(
                    out.status.success(),
                    "{shell} {target}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert_eq!(
                    fs::read(root.path().join("home/.local/bin/anticrumb.exe")).unwrap(),
                    BINARY
                );
                assert!(!root.path().join("outside").exists());
            }

            // Overwrite/upgrade an existing installation (verifying [IO.File]::Replace with
            // [NullString]::Value in Windows PowerShell 5.1 as well as pwsh) while leaving
            // sibling libraries untouched and no staged temp files behind.
            let overwrite_root = TempDir::new().unwrap();
            let bin_dir = overwrite_root.path().join("home/.local/bin");
            fs::create_dir_all(&bin_dir).unwrap();
            fs::write(bin_dir.join("anticrumb.exe"), b"previous binary").unwrap();
            fs::write(bin_dir.join("anticrumb.dll"), b"previous library").unwrap();
            let overwrite_out = run_ps(
                shell,
                &overwrite_root,
                "x86_64-pc-windows-msvc",
                PsOptions::default(),
            );
            assert!(
                overwrite_out.status.success(),
                "{shell} overwrite: {}",
                String::from_utf8_lossy(&overwrite_out.stderr)
            );
            assert_eq!(fs::read(bin_dir.join("anticrumb.exe")).unwrap(), BINARY);
            assert_eq!(
                fs::read(bin_dir.join("anticrumb.dll")).unwrap(),
                b"previous library"
            );
            let leftover_staged: Vec<_> = fs::read_dir(&bin_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".anticrumb-"))
                .collect();
            assert!(
                leftover_staged.is_empty(),
                "{shell} left staged files behind: {leftover_staged:?}"
            );

            let version_root = TempDir::new().unwrap();
            let version_tag = format!("v{VERSION}");
            let version_out = run_ps(
                shell,
                &version_root,
                "x86_64-pc-windows-msvc",
                PsOptions {
                    version: Some(&version_tag),
                    ..PsOptions::default()
                },
            );
            assert!(
                version_out.status.success(),
                "{shell} version: {}",
                String::from_utf8_lossy(&version_out.stderr)
            );
            assert_eq!(
                fs::read(version_root.path().join("home/.local/bin/anticrumb.exe")).unwrap(),
                BINARY
            );

            let nightly_root = TempDir::new().unwrap();
            let nightly_out = run_ps(
                shell,
                &nightly_root,
                "x86_64-pc-windows-msvc",
                PsOptions {
                    version: Some("nightly"),
                    ..PsOptions::default()
                },
            );
            assert!(
                nightly_out.status.success(),
                "{shell} nightly: {}",
                String::from_utf8_lossy(&nightly_out.stderr)
            );
            assert_eq!(
                fs::read(nightly_root.path().join("home/.local/bin/anticrumb.exe")).unwrap(),
                BINARY
            );

            let bad_hash_root = TempDir::new().unwrap();
            let existing_bin = bad_hash_root.path().join("home/.local/bin/anticrumb.exe");
            fs::create_dir_all(existing_bin.parent().unwrap()).unwrap();
            fs::write(&existing_bin, b"previous version").unwrap();
            assert!(!run_ps(
                shell,
                &bad_hash_root,
                "x86_64-pc-windows-msvc",
                PsOptions {
                    bad_hash: true,
                    ..PsOptions::default()
                },
            )
            .status
            .success());
            assert_eq!(fs::read(&existing_bin).unwrap(), b"previous version");

            let symlink_root = TempDir::new().unwrap();
            assert!(!run_ps(
                shell,
                &symlink_root,
                "x86_64-pc-windows-msvc",
                PsOptions {
                    symlink: true,
                    ..PsOptions::default()
                },
            )
            .status
            .success());
            assert!(!symlink_root
                .path()
                .join("home/.local/bin/anticrumb.exe")
                .exists());

            let non_prerelease_root = TempDir::new().unwrap();
            let reject_stable_on_nightly = |r: &mut Value| {
                r["prerelease"] = json!(false);
            };
            assert!(!run_ps(
                shell,
                &non_prerelease_root,
                "x86_64-pc-windows-msvc",
                PsOptions {
                    version: Some("nightly"),
                    mutate: Some(&reject_stable_on_nightly),
                    ..PsOptions::default()
                },
            )
            .status
            .success());
        }
    }
}
