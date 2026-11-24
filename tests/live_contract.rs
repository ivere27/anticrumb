//! Opt-in live Gemini regression checks; never runs in the default test suite.
//! Use ANTICRUMB_LIVE_PORT and optionally ANTICRUMB_LIVE_IMAGES=1 with --ignored.
use anticrumb::{
    agent::CancelToken,
    browser_host as browser,
    domain::*,
    service::{Backend, Emit, Service, WebBackend},
    store::Store,
    tab_pool,
};
use base64::Engine;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};
struct Observed {
    inner: WebBackend,
    active: AtomicUsize,
    max: AtomicUsize,
}
impl Backend for Observed {
    fn generate(
        &self,
        account: &Account,
        session: &mut Session,
        request: &TurnRequest,
        cancel: CancelToken,
        emit: Emit<'_>,
    ) -> Result<TurnResult> {
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        struct Done<'a>(&'a AtomicUsize);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _done = Done(&self.active);
        eprintln!(
            "LIVE backend_enter active={n} temporary={}",
            request.mode == SessionMode::Temporary
        );
        let mut forward = |kind: &str, data: serde_json::Value| {
            if kind == "submitted" || kind == "browser_stage" {
                eprintln!(
                    "LIVE event {kind} {}",
                    data.get("stage").and_then(|v| v.as_str()).unwrap_or("")
                );
            }
            emit(kind, data)
        };
        self.inner
            .generate(account, session, request, cancel, &mut forward)
    }
    fn close(&self, a: &Account, s: &Session) -> Result<()> {
        self.inner.close(a, s)
    }
    fn media(&self, a: &Account, s: &Session) -> Result<Vec<String>> {
        self.inner.media(a, s)
    }
}
struct Cleanup {
    store: Store,
    original: BTreeSet<String>,
    port: u16,
}

fn decoded_image_fingerprints(port: u16, images: &[&[u8]]) -> Result<Vec<String>> {
    struct Close(u16, String);
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = browser::close_tab(self.0, &self.1);
        }
    }
    let target = browser::create_tab(port, "about:blank").map_err(ServiceError::invalid)?;
    let _close = Close(port, target.id.clone());
    let mut client = anticrumb::cdp::CdpClient::connect(
        port,
        &browser::ws_path_from_url(&target.ws_url).map_err(ServiceError::invalid)?,
    )
    .map_err(ServiceError::invalid)?;
    client.set_control(
        CancelToken::new(),
        Instant::now() + std::time::Duration::from_secs(30),
    );
    images.iter().map(|bytes| {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let mime = if bytes.starts_with(b"\x89PNG") { "image/png" } else if bytes.starts_with(&[0xff,0xd8]) { "image/jpeg" } else { "image/webp" };
        let data_url = serde_json::to_string(&format!("data:{mime};base64,{encoded}")).map_err(ServiceError::storage)?;
        let value = client.call("Runtime.evaluate", serde_json::json!({
            "expression": format!("(async()=>{{const image=new Image();image.src={data_url};await image.decode();const canvas=document.createElement('canvas');canvas.width=image.naturalWidth;canvas.height=image.naturalHeight;canvas.getContext('2d').drawImage(image,0,0);return canvas.toDataURL('image/png');}})()"),
            "returnByValue":true,"awaitPromise":true,
        })).map_err(ServiceError::invalid)?;
        let canonical = value["result"]["value"].as_str().ok_or_else(|| ServiceError::invalid("could not decode generated image for live comparison"))?;
        Ok(digest(canonical.as_bytes()))
    }).collect()
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut targets: BTreeSet<String> = self
            .store
            .list::<Session>("local", "session")
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.id.starts_with("sentinel-"))
            .filter_map(|s| s.target_id)
            .collect();
        if let Ok(pool) = self
            .store
            .get::<tab_pool::TabPoolState>("default", "runtime", "tab_pool")
        {
            targets.extend(pool.slots.into_iter().map(|s| s.target_id));
        }
        let mut closed = 0;
        for target in targets.difference(&self.original) {
            if browser::close_tab(self.port, target).is_ok() {
                closed += 1;
            }
        }
        eprintln!("LIVE cleanup closed_test_tabs={closed}");
    }
}
async fn turn(service: &Arc<Service>, request: TurnRequest, label: &str) -> Result<TurnResult> {
    let start = Instant::now();
    let budget = std::time::Duration::from_secs(if request.generate_image { 240 } else { 120 });
    let id = service.submit(request)?;
    let run = match tokio::time::timeout(budget, service.wait(&id)).await {
        Ok(run) => run?,
        Err(_) => {
            service.cancel(&id)?;
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(10), service.wait(&id)).await;
            return Err(ServiceError::new(
                ErrorCode::DeadlineExceeded,
                "live_test",
                format!(
                    "{label} did not finish within {} seconds; cancelled without retry",
                    budget.as_secs()
                ),
            ));
        }
    };
    eprintln!(
        "LIVE {label} terminal={} elapsed_ms={}",
        run.state,
        start.elapsed().as_millis()
    );
    if let Some(error) = run.error {
        return Err(error);
    }
    run.result
        .ok_or_else(|| ServiceError::invalid("missing terminal result"))
}
fn req(prompt: &str, temporary: bool, image: bool) -> TurnRequest {
    let mut r = TurnRequest::text(prompt);
    r.retention = SessionRetention::Retain;
    r.timeout_secs = if image { 180 } else { 120 };
    r.mode = SessionMode::from_temporary(temporary);
    r.generate_image = image;
    r.model = std::env::var("ANTICRUMB_LIVE_MODEL").ok();
    r
}
async fn check(service: Arc<Service>, backend: Arc<Observed>, store: Store) -> Result<()> {
    let marker = format!("REVIEW_CONTEXT_{}", std::process::id());
    let first = turn(
        &service,
        req(
            &format!("Remember the exact token {marker}. Reply with this token only."),
            false,
            false,
        ),
        "prompt",
    )
    .await?;
    if !first.text.contains(&marker) {
        return Err(ServiceError::invalid(format!(
            "first prompt marker missing: {}",
            first.text
        )));
    }
    let mut follow=req("What exact token did I ask you to remember in the previous message? Reply only with that token.",false,false);
    follow.session_id = Some(first.session_id.clone());
    let second = turn(&service, follow, "followup").await?;
    if second.session_id != first.session_id || !second.text.contains(&marker) {
        return Err(ServiceError::invalid(format!(
            "followup context missing: {}",
            second.text
        )));
    }
    let s: Session = store.get("local", "session", &first.session_id)?;
    if s.remote_turns != 2 {
        return Err(ServiceError::invalid("followup remote turn count mismatch"));
    }
    println!("PASS real_prompt_and_context_followup");
    service.close_session(&first.session_id).await?;
    backend.max.store(0, Ordering::SeqCst);
    let a = req("Reply with exactly LIVE_WORKER_ALPHA_OK", true, false);
    let b = req("Reply with exactly LIVE_WORKER_BETA_OK", true, false);
    let (a, b) = tokio::join!(turn(&service, a, "worker_a"), turn(&service, b, "worker_b"));
    let (a, b) = (a?, b?);
    if !a.text.contains("LIVE_WORKER_ALPHA_OK") || !b.text.contains("LIVE_WORKER_BETA_OK") {
        return Err(ServiceError::invalid(format!(
            "worker results mismatch: {} / {}",
            a.text, b.text
        )));
    }
    let sa: Session = store.get("local", "session", &a.session_id)?;
    let sb: Session = store.get("local", "session", &b.session_id)?;
    let max = backend.max.load(Ordering::SeqCst);
    if max < 2 || sa.target_id == sb.target_id {
        return Err(ServiceError::invalid(format!(
            "parallel worker isolation failed: max={max}"
        )));
    }
    println!("PASS same_account_parallel_workers max_active={max} distinct_tabs=true");
    service.close_session(&a.session_id).await?;
    service.close_session(&b.session_id).await?;
    if std::env::var("ANTICRUMB_LIVE_IMAGES").as_deref() != Ok("1") {
        println!("SKIP real image generation (set ANTICRUMB_LIVE_IMAGES=1 to include)");
        return Ok(());
    }
    let a=turn(&service,req("Generate one simple square image: a solid blue circle centered on a white background, with no text or other objects.",false,true),"image_a").await?;
    if a.artifacts.is_empty() {
        return Err(ServiceError::invalid("image A has no artifacts"));
    }
    let sa: Session = store.get("local", "session", &a.session_id)?;
    let original = store.artifact_bytes("default", &a.artifacts[0])?.1;
    service.close_session(&a.session_id).await?;
    let b=turn(&service,req("Generate one simple square image: a solid red square centered on a white background, with no text or other objects.",false,true),"image_b").await?;
    if b.artifacts.is_empty() {
        return Err(ServiceError::invalid("image B has no artifacts"));
    }
    let sb: Session = store.get("local", "session", &b.session_id)?;
    if sa.target_id != sb.target_id {
        return Err(ServiceError::invalid(
            "media test did not exercise tab reuse",
        ));
    }
    let other = store.artifact_bytes("default", &b.artifacts[0])?.1;
    let recovered = service.recover_media(&a.session_id).await?;
    if recovered.is_empty() {
        return Err(ServiceError::invalid(
            "media recovery returned no artifacts",
        ));
    }
    let bytes = store.artifact_bytes("default", &recovered[0])?.1;
    // Gemini changes C2PA provenance metadata when an image is downloaded again.
    // Compare decoded pixels, retaining the requirement that A differs from B.
    let port = store.get::<Account>("local", "account", "default")?.port;
    let fingerprints = decoded_image_fingerprints(port, &[&original, &other, &bytes])?;
    let matches_original = fingerprints[0] == fingerprints[2];
    let matches_other = fingerprints[1] == fingerprints[2];
    let output = std::env::var_os("ANTICRUMB_LIVE_OUTPUT").map(PathBuf::from);
    if !matches_original || matches_other || output.is_some() {
        let diagnostic = if let Some(path) = output {
            let path = path.join(format!("media-{}", std::process::id()));
            std::fs::create_dir_all(&path).map_err(ServiceError::storage)?;
            path
        } else {
            tempfile::Builder::new()
                .prefix("anticrumb-live-media-")
                .tempdir()
                .map_err(ServiceError::storage)?
                .keep()
        };
        for (name, image) in [
            ("original", &original),
            ("other", &other),
            ("recovered", &bytes),
        ] {
            std::fs::write(diagnostic.join(format!("{name}.image")), image)
                .map_err(ServiceError::storage)?;
            eprintln!(
                "LIVE image {name} bytes={} sha256={}",
                image.len(),
                digest(image)
            );
        }
        eprintln!(
            "LIVE diagnostic images retained at {}",
            diagnostic.display()
        );
        if !matches_original || matches_other {
            return Err(ServiceError::invalid(format!("media pixels differ: matches_original={matches_original}, matches_other={matches_other}")));
        }
    }
    println!(
        "PASS real_image_recovery_after_tab_reuse bytes={}",
        bytes.len()
    );
    service.close_session(&b.session_id).await?;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "uses an explicitly selected signed-in Chrome account and sends real model prompts"]
async fn live_gemini_prompt_followup_workers_and_optional_media() {
    let result = async {
        let port: u16 = std::env::var("ANTICRUMB_LIVE_PORT")
            .expect("set ANTICRUMB_LIVE_PORT explicitly for live testing")
            .parse()
            .expect("ANTICRUMB_LIVE_PORT must be a port number");
        let meta = browser::inspect_running_cdp(port)
            .ok_or_else(|| ServiceError::invalid("selected live Chrome is unavailable"))?;
        let existing = browser::list_targets(port).map_err(ServiceError::invalid)?;
        let directory = tempfile::tempdir().map_err(ServiceError::storage)?;
        let store = Store::open(directory.path())?;
        let backend = Arc::new(Observed {
            inner: WebBackend::new(store.clone()),
            active: 0.into(),
            max: 0.into(),
        });
        let service = Service::new(store.clone(), backend.clone())?;
        let account = Account {
            id: "default".into(),
            provider: default_provider(),
            profile: PathBuf::from(meta.user_data_dir),
            subprofile: Some(meta.profile_dir),
            port,
            headless: meta.headless,
            state: "ready".into(),
        };
        store.put("local", "account", "default", &account)?;
        tab_pool::set_worker_tabs(Some(&store), 2);
        let original = existing.iter().map(|t| t.id.clone()).collect();
        for (i, t) in existing.iter().enumerate() {
            let s = Session {
                id: format!("sentinel-{i}"),
                provider: default_provider(),
                account_id: "default".into(),
                mode: SessionMode::Chat,
                target_id: Some(t.id.clone()),
                remote_url: Some(t.url.clone()),
                remote_session_id: None,
                history: vec![],
                state: SessionState::Ready,
                model: None,
                thinking: None,
                tool_digest: String::new(),
                remote_turns: 1,
                last_used: now_ms(),
                reuse_scope: None,
            };
            store.put("local", "session", &s.id, &s)?;
        }
        let _cleanup = Cleanup {
            store: store.clone(),
            original,
            port,
        };
        println!("LIVE existing_tabs_protected={}", existing.len());
        check(service, backend, store).await
    }
    .await;
    assert!(result.is_ok(), "live Gemini validation failed: {result:?}");
    println!("LIVE RESULT PASS");
}
