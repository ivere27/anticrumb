//! The protobuf contract is shared by the native module, IPC and network hosts.
pub mod pb {
    tonic::include_proto!("anticrumb.v1");
}
mod cache;
pub mod client;
pub mod codec;
pub mod connection;
pub mod daemon;
pub mod ffi;
pub(crate) mod fingerprint;
pub mod host;
mod host_sync;
pub(crate) mod local;
pub mod relay;
pub mod server;
pub mod server_cmd;
pub(crate) mod tls;
pub mod workspace_client;
pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;
pub const MAX_MESSAGE_LABEL: &str = "48 MiB";
pub const MAX_ARTIFACT: usize = crate::domain::MAX_ARTIFACT_BYTES as usize;
pub const ARTIFACT_CHUNK_BYTES: usize = 256 * 1024;

pub(super) trait ArtifactChunkSink: Send + 'static {
    type Output: Send + 'static;
    fn write_chunk(&mut self, bytes: &[u8]) -> crate::domain::Result<()>;
    fn finish(self) -> crate::domain::Result<Self::Output>;
}

impl ArtifactChunkSink for crate::store::ArtifactWriter {
    type Output = crate::domain::Artifact;
    fn write_chunk(&mut self, bytes: &[u8]) -> crate::domain::Result<()> {
        crate::store::ArtifactWriter::write_chunk(self, bytes)
    }
    fn finish(self) -> crate::domain::Result<Self::Output> {
        crate::store::ArtifactWriter::finish(self)
    }
}

pub(super) enum ChunkPayload {
    Meta,
    Data(Vec<u8>),
    Empty,
}

pub(super) struct ChunkStreamSpec {
    pub expected_size: u64,
    pub max_chunk_bytes: Option<usize>,
    pub duplicate_meta_msg: &'static str,
    pub empty_chunk_msg: &'static str,
    pub empty_data_msg: &'static str,
    pub chunk_limit_msg: std::borrow::Cow<'static, str>,
    pub overflow_msg: &'static str,
    pub size_mismatch_msg: &'static str,
    pub aborted_msg: &'static str,
}

pub(super) async fn stream_chunks_to_sink<C, S, W>(
    stream: S,
    sink: W,
    spec: ChunkStreamSpec,
    mut classify: impl FnMut(C) -> ChunkPayload,
) -> crate::domain::Result<W::Output>
where
    S: futures_util::Stream<Item = crate::domain::Result<C>>,
    W: ArtifactChunkSink,
{
    use futures_util::StreamExt;
    enum WriteMsg {
        Data(Vec<u8>),
        Finish,
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WriteMsg>(4);
    let aborted_msg = spec.aborted_msg;
    let mut writer_task =
        tokio::task::spawn_blocking(move || -> crate::domain::Result<W::Output> {
            let mut sink = Some(sink);
            let mut finished = false;
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    WriteMsg::Data(chunk) => {
                        let s = sink.as_mut().ok_or_else(|| {
                            crate::domain::ServiceError::storage("artifact writer missing")
                        })?;
                        s.write_chunk(&chunk)?;
                    }
                    WriteMsg::Finish => {
                        finished = true;
                        break;
                    }
                }
            }
            if !finished {
                return Err(crate::domain::ServiceError::invalid(aborted_msg));
            }
            sink.take()
                .ok_or_else(|| crate::domain::ServiceError::storage("artifact writer missing"))?
                .finish()
        });

    tokio::pin!(stream);
    let mut received = 0u64;
    let stream_result: crate::domain::Result<()> = loop {
        match stream.next().await {
            None => break Ok(()),
            Some(Err(err)) => break Err(err),
            Some(Ok(chunk)) => match classify(chunk) {
                ChunkPayload::Meta => {
                    break Err(crate::domain::ServiceError::invalid(
                        spec.duplicate_meta_msg,
                    ));
                }
                ChunkPayload::Empty => {
                    break Err(crate::domain::ServiceError::invalid(spec.empty_chunk_msg));
                }
                ChunkPayload::Data(data) => {
                    if let Some(limit) = spec.max_chunk_bytes {
                        if data.len() > limit {
                            break Err(crate::domain::ServiceError::invalid(
                                spec.chunk_limit_msg.clone(),
                            ));
                        }
                    }
                    if data.is_empty() {
                        break Err(crate::domain::ServiceError::invalid(spec.empty_data_msg));
                    }
                    let Some(next) = received.checked_add(data.len() as u64) else {
                        break Err(crate::domain::ServiceError::invalid(spec.overflow_msg));
                    };
                    if next > spec.expected_size {
                        break Err(crate::domain::ServiceError::invalid(spec.overflow_msg));
                    }
                    received = next;
                    if tx.send(WriteMsg::Data(data)).await.is_err() {
                        let err = match (&mut writer_task).await {
                            Ok(Err(e)) => e,
                            Ok(Ok(_)) => {
                                crate::domain::ServiceError::storage("writer task ended early")
                            }
                            Err(e) => crate::domain::ServiceError::storage(e),
                        };
                        return Err(err);
                    }
                }
            },
        }
    };
    if let Err(err) = stream_result {
        drop(tx);
        let _ = writer_task.await;
        return Err(err);
    }
    if received != spec.expected_size {
        drop(tx);
        let _ = writer_task.await;
        return Err(crate::domain::ServiceError::invalid(spec.size_mismatch_msg));
    }
    if tx.send(WriteMsg::Finish).await.is_err() {
        let err = match (&mut writer_task).await {
            Ok(Err(e)) => e,
            Ok(Ok(_)) => crate::domain::ServiceError::storage("writer task ended early"),
            Err(e) => crate::domain::ServiceError::storage(e),
        };
        return Err(err);
    }
    drop(tx);
    writer_task
        .await
        .map_err(crate::domain::ServiceError::storage)?
}

pub(crate) fn validate_token(token: &str) -> crate::domain::Result<()> {
    if !(32..=4096).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(crate::domain::ServiceError::invalid(
            "token must contain 32..4096 printable non-whitespace ASCII characters",
        ));
    }
    Ok(())
}

pub(crate) fn constant_time_eq(provided: &str, expected: &str) -> bool {
    let mismatch = provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b));
    provided.len() == expected.len() && mismatch == 0
}

pub fn private_write(path: &std::path::Path, data: &[u8]) -> crate::domain::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(crate::domain::ServiceError::storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(crate::domain::ServiceError::storage)?;
        }
        file.write_all(data)
            .map_err(crate::domain::ServiceError::storage)?;
        file.sync_all()
            .map_err(crate::domain::ServiceError::storage)?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(crate::domain::ServiceError::storage)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
