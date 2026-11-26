//! Statically linked Synurang module API; no dynamic library or socket is involved.
use super::{codec, host::Host, pb};
use prost::Message;
use std::{cell::RefCell, sync::Arc};
#[allow(unused_imports, dead_code)]
mod generated {
    use super::pb::*;
    include!("generated/anticrumb_ffi.rs");
}
struct FfiApi {
    host: Arc<Host>,
    runtime: tokio::runtime::Handle,
}
struct AbortTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
trait FfiServerStream: Sized + Send + 'static {
    type Item: Message + Default + Send + 'static;
    fn open(
        self,
        host: Arc<Host>,
        path: String,
    ) -> impl std::future::Future<
        Output = crate::domain::Result<
            tokio::sync::mpsc::Receiver<crate::domain::Result<Self::Item>>,
        >,
    > + Send;
}

impl FfiServerStream for pb::EventCursor {
    type Item = pb::Event;
    async fn open(
        self,
        host: Arc<Host>,
        path: String,
    ) -> crate::domain::Result<tokio::sync::mpsc::Receiver<crate::domain::Result<Self::Item>>> {
        if path == "/anticrumb.v1.WorkspaceService/WatchAgentEvents" {
            host.watch_agent(self).await
        } else {
            host.watch(self).await
        }
    }
}

impl FfiServerStream for pb::ArtifactRef {
    type Item = pb::GetArtifactChunk;
    async fn open(
        self,
        host: Arc<Host>,
        _path: String,
    ) -> crate::domain::Result<tokio::sync::mpsc::Receiver<crate::domain::Result<Self::Item>>> {
        host.get_artifact_stream(self).await
    }
}

trait FfiClientStream: Message + Default + Send + 'static {
    type Resp: Message + Default + Send + 'static;
    fn handle(
        host: Arc<Host>,
        stream: tokio_stream::wrappers::ReceiverStream<crate::domain::Result<Self>>,
    ) -> impl std::future::Future<Output = crate::domain::Result<Self::Resp>> + Send;
}

impl FfiClientStream for pb::PutArtifactChunk {
    type Resp = pb::Artifact;
    async fn handle(
        host: Arc<Host>,
        stream: tokio_stream::wrappers::ReceiverStream<crate::domain::Result<Self>>,
    ) -> crate::domain::Result<Self::Resp> {
        host.put_artifact_stream(stream).await
    }
}

impl FfiApi {
    async fn call<Req: Message, Resp: Message + Default>(
        &self,
        context: synurang_call::Context,
        path: &str,
        request: Req,
    ) -> synurang_call::Result<Resp> {
        let host = self.host.clone();
        let path = path.to_string();
        let bytes = request.encode_to_vec();
        let mut task = AbortTask(
            self.runtime
                .spawn(async move { host.dispatch(&path, bytes).await }),
        );
        let bytes = tokio::select! {
            error=context.cancelled()=>return Err(error),
            result=&mut task.0=>result.map_err(|e|synurang_call::RpcError::new(13,e.to_string()))?.map_err(codec::module_error)?,
        };
        Resp::decode(bytes.as_slice()).map_err(|e| synurang_call::RpcError::new(13, e.to_string()))
    }
    async fn stream<Req: FfiServerStream>(
        &self,
        context: synurang_call::Context,
        path: &str,
        request: Req,
        responses: synurang_call::Sender<Req::Item>,
    ) -> synurang_call::Result<()> {
        let host = self.host.clone();
        let path = path.to_owned();
        let mut task = AbortTask(self.runtime.spawn(async move { request.open(host, path).await }));
        let mut receiver = tokio::select! {
            error=context.cancelled()=>return Err(error),
            result=&mut task.0=>result.map_err(|e|synurang_call::RpcError::new(13,e.to_string()))?.map_err(codec::module_error)?,
        };
        loop {
            let next = tokio::select! {error=context.cancelled()=>return Err(error),value=receiver.recv()=>value};
            let Some(value) = next else {
                return Ok(());
            };
            responses.send(value.map_err(codec::module_error)?).await?;
        }
    }
    async fn client_stream<Req: FfiClientStream>(
        &self,
        context: synurang_call::Context,
        _path: &str,
        mut requests: synurang_call::Receiver<Req>,
    ) -> synurang_call::Result<Req::Resp> {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let mut tx = Some(tx);
        let host = self.host.clone();
        let mut task = AbortTask(self.runtime.spawn(async move {
            Req::handle(host, tokio_stream::wrappers::ReceiverStream::new(rx)).await
        }));
        let mut pending: Option<Req> = None;
        loop {
            if pending.is_none() && tx.is_none() {
                return tokio::select! {
                    error = context.cancelled() => {
                        abort_client_stream::<Req>(None, &mut task, error).await
                    }
                    result = &mut task.0 => result
                        .map_err(|e| synurang_call::RpcError::new(13, e.to_string()))?
                        .map_err(codec::module_error),
                };
            }
            if pending.is_some() {
                enum Step<R> {
                    Cancelled(synurang_call::RpcError),
                    Finished(R),
                    Sent,
                    Closed,
                }
                let step = {
                    let sender = tx.as_ref().expect("sender present while item pending");
                    tokio::select! {
                        error = context.cancelled() => Step::Cancelled(error),
                        result = &mut task.0 => Step::Finished(result),
                        permit = sender.reserve() => match permit {
                            Ok(permit) => {
                                permit.send(Ok(pending.take().unwrap()));
                                Step::Sent
                            }
                            Err(_) => Step::Closed,
                        }
                    }
                };
                match step {
                    Step::Cancelled(error) => {
                        return abort_client_stream(tx.take(), &mut task, error).await;
                    }
                    Step::Finished(result) => {
                        return result
                            .map_err(|e| synurang_call::RpcError::new(13, e.to_string()))?
                            .map_err(codec::module_error);
                    }
                    Step::Closed => {
                        pending = None;
                        tx.take();
                    }
                    Step::Sent => {}
                }
            } else {
                tokio::select! {
                    error = context.cancelled() => {
                        return abort_client_stream(tx.take(), &mut task, error).await;
                    }
                    result = &mut task.0 => return result
                        .map_err(|e| synurang_call::RpcError::new(13, e.to_string()))?
                        .map_err(codec::module_error),
                    next = requests.recv() => match next {
                        Ok(Some(item)) => pending = Some(item),
                        Ok(None) => {
                            tx.take();
                        }
                        Err(error) => {
                            return abort_client_stream(tx.take(), &mut task, error).await;
                        }
                    }
                }
            }
        }
    }
}

async fn abort_client_stream<Req: FfiClientStream>(
    tx: Option<tokio::sync::mpsc::Sender<crate::domain::Result<Req>>>,
    task: &mut AbortTask<crate::domain::Result<Req::Resp>>,
    error: synurang_call::RpcError,
) -> synurang_call::Result<Req::Resp> {
    if let Some(sender) = tx {
        let _ = sender
            .send(Err(crate::domain::ServiceError::new(
                crate::domain::ErrorCode::Cancelled,
                "ffi",
                error.to_string(),
            )))
            .await;
        drop(sender);
    }
    let _ = (&mut task.0).await;
    Err(error)
}
impl Drop for FfiApi {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}
include!(concat!(env!("OUT_DIR"), "/ffi_bindings.rs"));
thread_local! {
    // The static ABI create callback is synchronous and has no user-data slot.
    // This scoped handoff is consumed on the creating thread, never process-global.
    static PREPARED: RefCell<Option<Arc<FfiApi>>> = const { RefCell::new(None) };
}

pub(super) fn embedded(
    host: Arc<Host>,
    runtime: tokio::runtime::Handle,
) -> crate::domain::Result<synurang_host::ModuleHost> {
    let _entered = runtime.enter();
    let prepared = Arc::new(FfiApi {
        host,
        runtime: runtime.clone(),
    });
    PREPARED.with(|slot| -> crate::domain::Result<()> {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(crate::domain::ServiceError::invalid(
                "nested embedded module initialization",
            ));
        }
        *slot = Some(prepared);
        Ok(())
    })?;
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            PREPARED.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    let _clear = Clear;
    // Both crates use the pinned Synurang C ABI. Its table and functions are
    // linked into this executable and remain valid for the process lifetime.
    unsafe {
        synurang_host::ModuleHost::from_static_api(Anticrumb_GetApi().cast(), Default::default())
    }
    .map_err(codec::from_module)
}

fn factory() -> synurang_call::Instance {
    let api = PREPARED
        .with(|slot| slot.borrow_mut().take())
        .expect("prepared embedded host");
    let mut instance = synurang_call::Instance::default();
    generated::register_platform_service(&mut instance, api.clone()).unwrap();
    generated::register_provider_service(&mut instance, api.clone()).unwrap();
    generated::register_account_service(&mut instance, api.clone()).unwrap();
    generated::register_model_service(&mut instance, api.clone()).unwrap();
    generated::register_turn_service(&mut instance, api.clone()).unwrap();
    generated::register_session_service(&mut instance, api.clone()).unwrap();
    generated::register_artifact_service(&mut instance, api.clone()).unwrap();
    if api.host.role == "workspace" {
        generated::register_workspace_service(&mut instance, api).unwrap();
    }
    instance
}
synurang_call::export_module!(Anticrumb_GetApi, factory);
