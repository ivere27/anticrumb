use prost::Message;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn format_utc_timestamp(epoch_secs: u64) -> String {
    let secs_of_day = epoch_secs % 86400;
    let hours = secs_of_day / 3600;
    let minutes = (secs_of_day % 3600) / 60;
    let seconds = secs_of_day % 60;

    let z = (epoch_secs / 86400) as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}:{seconds:02} UTC")
}

fn snake(name: &str) -> String {
    let mut output = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && index != 0 {
            output.push('_');
        }
        output.extend(ch.to_lowercase());
    }
    output
}
fn ty(name: &str) -> &str {
    name.rsplit('.').next().unwrap()
}
fn git_is_dirty() -> bool {
    let unstaged = std::process::Command::new("git")
        .args(["diff", "--no-ext-diff", "--quiet"])
        .status()
        .is_ok_and(|s| s.code() == Some(1));
    if unstaged {
        return true;
    }
    std::process::Command::new("git")
        .args(["diff", "--no-ext-diff", "--cached", "--quiet"])
        .status()
        .is_ok_and(|s| s.code() == Some(1))
}

fn resolve_git_head() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if git_head.is_empty() {
        return None;
    }
    if git_is_dirty() {
        Some(format!("{git_head}-dirty"))
    } else {
        Some(git_head)
    }
}

fn git_path(rel: &str) -> PathBuf {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", rel])
        .output()
    {
        if output.status.success() {
            let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    Path::new(".git").join(rel)
}

fn emit_git_rerun_hints() {
    let head_path = git_path("HEAD");
    if let Ok(head) = fs::read_to_string(&head_path) {
        println!("cargo:rerun-if-changed={}", head_path.display());
        if let Some(ref_path) = head.strip_prefix("ref: ").map(str::trim) {
            let full_ref = git_path(ref_path);
            if full_ref.exists() {
                println!("cargo:rerun-if-changed={}", full_ref.display());
            }
        }
    }
    for rel in ["index", "refs/heads", "packed-refs"] {
        let path = git_path(rel);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Ok(output) = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .output()
    {
        if output.status.success() {
            for raw in output.stdout.split(|&b| b == 0) {
                if raw.is_empty() {
                    continue;
                }
                if let Ok(path) = std::str::from_utf8(raw) {
                    if !path.contains(['\n', '\r']) {
                        println!("cargo:rerun-if-changed={path}");
                    }
                }
            }
        }
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

fn resolve_build_channel() -> &'static str {
    match env::var("ANTICRUMB_BUILD_CHANNEL") {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "" | "dev" => "dev",
            "nightly" => "nightly",
            "stable" => "stable",
            other => panic!(
                "invalid ANTICRUMB_BUILD_CHANNEL {other:?} (expected stable, nightly, or dev)"
            ),
        },
        Err(env::VarError::NotPresent) => "dev",
        Err(env::VarError::NotUnicode(_)) => {
            panic!("ANTICRUMB_BUILD_CHANNEL must be valid UTF-8 (stable, nightly, or dev)")
        }
    }
}

fn main() {
    println!(
        "cargo:rustc-env=ANTICRUMB_BUILD_TARGET={}",
        env::var("TARGET").expect("Cargo build target")
    );
    println!("cargo:rerun-if-env-changed=ANTICRUMB_BUILD_CHANNEL");
    println!(
        "cargo:rustc-env=ANTICRUMB_BUILD_CHANNEL={}",
        resolve_build_channel()
    );
    let build_time = env::var("ANTICRUMB_BUILD_TIME")
        .or_else(|_| {
            env::var("SOURCE_DATE_EPOCH")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .map(format_utc_timestamp)
                .ok_or(env::VarError::NotPresent)
        })
        .unwrap_or_else(|_| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format_utc_timestamp(secs)
        });
    println!("cargo:rustc-env=ANTICRUMB_BUILD_TIME={build_time}");

    println!("cargo:rerun-if-env-changed=ANTICRUMB_GIT_HEAD");
    if let Some(git_head) = env::var("ANTICRUMB_GIT_HEAD")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "unknown")
    {
        println!("cargo:rustc-env=ANTICRUMB_GIT_HEAD={git_head}");
    } else if let Some(git_head) = resolve_git_head() {
        println!("cargo:rustc-env=ANTICRUMB_GIT_HEAD={git_head}");
    }
    emit_git_rerun_hints();
    println!("cargo:rerun-if-changed=proto/anticrumb.proto");
    println!("cargo:rerun-if-changed=src/rpc/generated/anticrumb_ffi.rs");
    use sha2::{Digest, Sha256};
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read("src/rpc/generated/manifest.json").expect("run make proto first"),
    )
    .unwrap();
    let schema = canonical_lf(&fs::read("proto/anticrumb.proto").unwrap());
    let schema_hex: String = Sha256::digest(&schema)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        manifest["schema_sha256"].as_str().unwrap(),
        schema_hex,
        "proto changed: run make proto with the pinned Synurang release generator"
    );
    println!("cargo:rerun-if-changed=src/rpc/generated/manifest.json");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let descriptor = output.join("anticrumb.bin");
    tonic_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .message_attribute(".", "#[serde(default)]")
        .file_descriptor_set_path(&descriptor)
        .compile_protos(&["proto/anticrumb.proto"], &["proto"])
        .expect("generate the single API contract");
    let descriptors =
        prost_types::FileDescriptorSet::decode(fs::read(descriptor).unwrap().as_slice()).unwrap();
    let mut grpc = String::new();
    let mut ffi = String::new();
    let mut clients = String::new();
    for file in descriptors.file {
        let package = file.package.unwrap();
        for service in file.service {
            let name = service.name.unwrap();
            grpc.push_str(&format!(
                "#[tonic::async_trait]\nimpl pb::{}_server::{} for GrpcApi {{\n",
                snake(&name),
                name
            ));
            ffi.push_str(&format!("impl generated::{}Service for FfiApi {{\n", name));
            for method in service.method {
                let method_name = method.name.unwrap();
                let rust_name = snake(&method_name);
                let request = ty(method.input_type.as_ref().unwrap());
                let response = ty(method.output_type.as_ref().unwrap());
                let path = format!("/{package}.{name}/{method_name}");
                let client_stream = method.client_streaming.unwrap_or(false);
                let server_stream = method.server_streaming.unwrap_or(false);
                assert!(
                    !(client_stream && server_stream),
                    "add bidi-stream codegen before declaring a bidi stream"
                );
                if server_stream {
                    grpc.push_str(&format!("type {method_name}Stream = TypedStream<pb::{response}>;\nasync fn {rust_name}(&self, request: tonic::Request<pb::{request}>) -> std::result::Result<tonic::Response<Self::{method_name}Stream>,tonic::Status> {{ let api = self.for_request(request.extensions()); api.authorize(request.metadata())?; api.stream(\"{path}\", request.into_inner()).await }}\n"));
                    ffi.push_str(&format!("async fn {rust_name}(&self, context: synurang_call::Context, request: pb::{request}, responses: synurang_call::Sender<pb::{response}>) -> synurang_call::Result<()> {{ self.stream(context,\"{path}\",request,responses).await }}\n"));
                } else if client_stream {
                    grpc.push_str(&format!("async fn {rust_name}(&self, request: tonic::Request<tonic::Streaming<pb::{request}>>) -> std::result::Result<tonic::Response<pb::{response}>,tonic::Status> {{ let api = self.for_request(request.extensions()); api.authorize(request.metadata())?; api.client_stream(\"{path}\", request.into_inner()).await.map(tonic::Response::new) }}\n"));
                    ffi.push_str(&format!("async fn {rust_name}(&self, context: synurang_call::Context, requests: synurang_call::Receiver<pb::{request}>) -> synurang_call::Result<pb::{response}> {{ self.client_stream(context,\"{path}\",requests).await }}\n"));
                } else {
                    grpc.push_str(&format!("async fn {rust_name}(&self, request: tonic::Request<pb::{request}>) -> std::result::Result<tonic::Response<pb::{response}>,tonic::Status> {{ let api = self.for_request(request.extensions()); api.authorize(request.metadata())?; api.call(\"{path}\",request.into_inner()).await.map(tonic::Response::new) }}\n"));
                    ffi.push_str(&format!("async fn {rust_name}(&self, context: synurang_call::Context, request: pb::{request}) -> synurang_call::Result<pb::{response}> {{ self.call(context,\"{path}\",request).await }}\n"));
                    clients.push_str(&format!("pub async fn {rust_name}(&self, request: pb::{request}) -> crate::domain::Result<pb::{response}> {{ self.unary(\"{path}\",request).await }}\n"));
                }
            }
            grpc.push_str("}\n");
            ffi.push_str("}\n");
        }
    }
    fs::write(output.join("grpc_bindings.rs"), grpc).unwrap();
    fs::write(output.join("ffi_bindings.rs"), ffi).unwrap();
    fs::write(
        output.join("client_bindings.rs"),
        format!("impl RpcClient {{\n{clients}}}\n"),
    )
    .unwrap();
}
