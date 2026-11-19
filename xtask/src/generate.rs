use crate::archive::{extract_single_file, sha256_bytes};
use serde_json::json;
use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::Command;

const VERSION: &str = "0.8.0";
const COMMIT: &str = "53180b484cf7ca07a1e7d6f24e58b8a19a2dcfa8";

fn asset_spec() -> Result<(&'static str, &'static str, &'static str, &'static str), String> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let norm_arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return Err(format!("No pinned release generator for {os}/{arch}")),
    };
    match (os, norm_arch) {
        ("windows", "x86_64") => Ok((
            "x86_64-pc-windows-gnu",
            "zip",
            "07226a91ba8cbaf7cf996824612231d5d818967d974bd0700e760dc5e50617d9",
            "amd64",
        )),
        ("windows", _) => {
            Err("Use an x86_64 Windows or Linux build host for the release generator.".to_string())
        }
        ("linux" | "macos", "x86_64") => Ok((
            "x86_64-unknown-linux-musl",
            "tar.gz",
            "759148a4a1568f73c39d8fa127c71204226ef56a1cdab53ec0778b69e6f9db0b",
            "linux/amd64",
        )),
        ("linux" | "macos", "aarch64") => Ok((
            "aarch64-unknown-linux-musl",
            "tar.gz",
            "7f5196ce1eb0818d7b6993084922bb038c07d9468b931ee3ce6d0b49dffe294d",
            "linux/arm64",
        )),
        _ => Err(format!("No pinned release generator for {os}/{arch}")),
    }
}

fn canonical_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

pub fn run(root: &Path) -> Result<(), String> {
    let (target, suffix, digest, docker_platform) = asset_spec()?;
    let filename = format!("protoc-gen-synurang-ffi-{VERSION}-{target}.{suffix}");
    let directory = root.join(".tools/synurang").join(VERSION).join(target);
    fs::create_dir_all(&directory).map_err(|e| format!("mkdir {}: {e}", directory.display()))?;
    let archive = directory.join(&filename);
    if !archive.exists() {
        let url =
            format!("https://github.com/ivere27/synurang/releases/download/v{VERSION}/{filename}");
        let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
        let tmp = directory.join(format!("{filename}.download"));
        let status = Command::new(curl)
            .args(["-fsSL", "--max-time", "60", &url, "-o"])
            .arg(&tmp)
            .status()
            .map_err(|e| format!("failed to run {curl}: {e}"))?;
        if !status.success() {
            let _ = fs::remove_file(&tmp);
            return Err(format!("{curl} failed downloading {url}"));
        }
        let downloaded = fs::read(&tmp).map_err(|e| format!("read {}: {e}", tmp.display()))?;
        if sha256_bytes(&downloaded) != digest {
            let _ = fs::remove_file(&tmp);
            return Err("Synurang release archive checksum mismatch".to_string());
        }
        fs::rename(&tmp, &archive).map_err(|e| format!("rename {}: {e}", archive.display()))?;
    }

    let data = fs::read(&archive).map_err(|e| format!("read {}: {e}", archive.display()))?;
    if sha256_bytes(&data) != digest {
        return Err("Cached Synurang release archive checksum mismatch".to_string());
    }

    let bin_name = if cfg!(windows) {
        "protoc-gen-synurang-ffi.exe"
    } else {
        "protoc-gen-synurang-ffi"
    };
    let binary_bytes = extract_single_file(&data, suffix, bin_name)?;
    let executable = directory.join(bin_name);
    {
        let mut f = File::create(&executable)
            .map_err(|e| format!("write {}: {e}", executable.display()))?;
        std::io::Write::write_all(&mut f, &binary_bytes)
            .map_err(|e| format!("write {}: {e}", executable.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", executable.display()))?;
    }

    let mut plugin = executable;
    if env::consts::OS == "macos" {
        let docker_ok = Command::new("docker")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !docker_ok {
            return Err(
                "The pinned release has no macOS binary. Install Docker to regenerate; normal builds use checked-in glue."
                    .to_string(),
            );
        }
        let wrapper = directory.join("protoc-gen-synurang-docker");
        fs::write(
            &wrapper,
            format!(
                "#!/usr/bin/env sh\nexec docker run --rm -i --platform {docker_platform} -v \"{}:/generator:ro\" alpine:3.21 \"/generator/{bin_name}\"\n",
                directory.display()
            ),
        )
        .map_err(|e| format!("write {}: {e}", wrapper.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", wrapper.display()))?;
        }
        plugin = wrapper;
    }

    let protoc = env::var("PROTOC").unwrap_or_else(|_| "protoc".to_string());
    let proto_dir = root.join("proto");
    let output_dir = root.join("src/rpc/generated");
    let status = Command::new(&protoc)
        .arg("--experimental_allow_proto3_optional")
        .arg("-I")
        .arg(&proto_dir)
        .arg(format!(
            "--plugin=protoc-gen-synurang-ffi={}",
            plugin.display()
        ))
        .arg(format!(
            "--synurang-ffi_out=lang=rust,mode=module:{}",
            output_dir.display()
        ))
        .arg(proto_dir.join("anticrumb.proto"))
        .status()
        .map_err(|e| format!("failed to run {protoc}: {e}"))?;
    if !status.success() {
        return Err(format!(
            "{protoc} exited with status {}",
            status.code().unwrap_or(1)
        ));
    }

    let schema = fs::read(proto_dir.join("anticrumb.proto"))
        .map_err(|e| format!("read anticrumb.proto: {e}"))?;
    let manifest = json!({
        "generator_release": format!("v{VERSION}"),
        "runtime_commit": COMMIT,
        "schema_sha256": sha256_bytes(&canonical_lf(&schema)),
    });
    let mut rendered =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    rendered.push('\n');
    fs::write(output_dir.join("manifest.json"), rendered)
        .map_err(|e| format!("write manifest.json: {e}"))?;

    println!("Generated anticrumb glue with verified Synurang v{VERSION} release");
    Ok(())
}

pub fn run_cli(root: &Path) -> i32 {
    if let Err(err) = run(root) {
        eprintln!("{err}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_lf_normalizes_crlf_for_schema_hashing() {
        let lf = b"syntax = \"proto3\";\nmessage Foo {}\n";
        let crlf = b"syntax = \"proto3\";\r\nmessage Foo {}\r\n";
        assert_eq!(
            sha256_bytes(&canonical_lf(lf)),
            sha256_bytes(&canonical_lf(crlf))
        );
        assert_eq!(canonical_lf(crlf), lf);
    }
}
