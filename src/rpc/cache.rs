use super::{host::Host, pb};
use crate::{
    domain::*,
    store::cache::{Options, DEFAULT_DAYS, INTERVAL},
};
use std::sync::Arc;

impl Host {
    pub(super) fn start_cache_maintenance(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        let cancel = self.cache_cancel.clone();
        runtime.spawn(async move {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let Some(host) = weak.upgrade() else {
                    break;
                };
                let store = host.service.store.clone();
                drop(host);
                if let Err(error) = store.blocking(|store| store.auto_clean_cache()).await {
                    eprintln!("cache maintenance: {error}");
                }
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(INTERVAL) => {},
                }
            }
        });
    }

    pub(super) async fn manage_cache(&self, request: pb::CacheRequest) -> Result<pb::CacheReport> {
        let options = Options {
            days: if request.days == 0 {
                DEFAULT_DAYS
            } else {
                request.days
            },
            prune: request.prune,
            dry_run: request.dry_run,
        };
        options.validate()?;
        let mut local = pb::CacheStoreReport {
            role: self.role.clone(),
            server_id: self.server_id.clone(),
            ..Default::default()
        };
        match self
            .service
            .store
            .blocking(move |store| store.cache_report(options))
            .await
        {
            Ok(stats) => local.stats = Some(stats_to(stats)),
            Err(error) => local.error = error.to_string(),
        }
        let mut stores = vec![local];
        if let Some(remote) = &self.remote {
            let result = async {
                let info = remote.get_info(pb::Empty {}).await?;
                if info.role != "browser"
                    || self
                        .expected_browser_server_id()
                        .is_some_and(|expected| expected != info.server_id)
                {
                    return Err(ServiceError::invalid(
                        "browser daemon identity changed; reconnect before cache maintenance",
                    ));
                }
                remote.manage_cache(request).await
            }
            .await;
            match result {
                Ok(report) => stores.extend(report.stores),
                Err(error) => stores.push(pb::CacheStoreReport {
                    role: "browser".into(),
                    error: error.to_string(),
                    ..Default::default()
                }),
            }
        }
        Ok(pb::CacheReport { stores })
    }
}

fn stats_to(stats: CacheStats) -> pb::CacheStats {
    pb::CacheStats {
        retention_days: stats.retention_days,
        blob_bytes: stats.blob_bytes,
        temporary_bytes: stats.temporary_bytes,
        reclaimable_bytes: stats.reclaimable_bytes,
        reclaimed_bytes: stats.reclaimed_bytes,
        eligible_artifacts: stats.eligible_artifacts,
        temporary_entries: stats.temporary_entries,
        orphan_blobs: stats.orphan_blobs,
        expired_cache: stats.expired_cache,
        last_cleanup: stats.last_cleanup,
        pruned: stats.pruned,
        busy: stats.busy,
        pruned_events: stats.pruned_events,
        compacted_turns: stats.compacted_turns,
    }
}
