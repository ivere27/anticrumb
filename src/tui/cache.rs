use crate::{
    domain::now_ms,
    rpc::pb,
    store::cache::{Options, BLOB_LIMIT},
};

fn size(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

pub(super) fn report(report: &pb::CacheReport, options: Options) -> String {
    let action = if options.dry_run {
        "preview"
    } else if options.prune {
        "cleanup"
    } else {
        "storage"
    };
    let mut lines = vec![format!(
        "Cache {action} · {} day retention · temporary files: 24 hours",
        options.days
    )];
    for store in &report.stores {
        if !store.error.is_empty() {
            lines.push(format!(
                "{}: unavailable / incomplete — {}",
                store.role, store.error
            ));
            continue;
        }
        let Some(stats) = &store.stats else {
            lines.push(format!("{}: no storage report", store.role));
            continue;
        };
        lines.push(format!(
            "{}: blobs {} / {}, temporary {}",
            store.role,
            size(stats.blob_bytes),
            size(BLOB_LIMIT),
            size(stats.temporary_bytes)
        ));
        lines.push(if stats.pruned {
            format!("  Reclaimed {} · {} artifacts, {} temporary directories, {} orphan files, {} expired cache entries",
                size(stats.reclaimed_bytes), stats.eligible_artifacts, stats.temporary_entries, stats.orphan_blobs, stats.expired_cache)
        } else {
            format!("  Reclaimable {} · {} artifacts, {} temporary directories, {} orphan files, {} expired cache entries",
                size(stats.reclaimable_bytes), stats.eligible_artifacts, stats.temporary_entries, stats.orphan_blobs, stats.expired_cache)
        });
        lines.push(format!(
            "  Last cleanup: {}",
            if stats.last_cleanup == 0 {
                "never".into()
            } else {
                format!(
                    "{} minutes ago",
                    now_ms().saturating_sub(stats.last_cleanup).max(0) / 60_000
                )
            }
        ));
        if stats.busy {
            lines.push(
                "  Active work: attachment deletion deferred; temporary files in use are retained."
                    .into(),
            );
        }
        if stats.blob_bytes >= BLOB_LIMIT * 8 / 10 {
            lines.push("  Storage is near its limit. Referenced files are retained; review saved attachments or free disk space.".into());
        }
    }
    lines.push("Automatic checks: startup and every 6 hours, with 7-day retention. Saved conversations and referenced attachments are retained.".into());
    if options.dry_run {
        lines.push("Preview only; no files deleted by this request.".into());
    }
    lines.join("\n")
}
