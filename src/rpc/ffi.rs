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
    async fn stream(
        &self,
        context: synurang_call::Context,
        _path: &str,
        request: pb::EventCursor,
        responses: synurang_call::Sender<pb::Event>,
    ) -> synurang_call::Result<()> {
        let host = self.host.clone();
        let mut task = AbortTask(self.runtime.spawn(async move { host.watch(request).await }));
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
