//! Reproduce open/screenshot failures in an isolated public browser without a model.
use anticrumb::{
    agent::CancelToken,
    browser_control::{self, BrowserControlRegistry},
    domain::Account,
};
use serde_json::json;
use std::{io::Write, process::ExitCode};

fn main() -> ExitCode {
    let url = std::env::args()
        .nth(1)
        .expect("Usage: browser_control_diagnostic URL");
    assert!(
        !anticrumb::web::allow_private_env(),
        "This diagnostic requires the default public browsing policy"
    );
    let account = Account {
        id: "browser-diagnostic".into(),
        provider: "gemini-web".into(),
        profile: std::env::temp_dir().join("anticrumb-unused-diagnostic-profile"),
        subprofile: None,
        port: 0,
        headless: true,
        state: "ready".into(),
    };
    let registry = BrowserControlRegistry::default();
    let mut failed = false;
    for args in [
        json!({"action":"open", "url":url}),
        json!({"action":"screenshot"}),
        json!({"action":"tabs"}),
    ] {
        let request = browser_control::request(&args, &account.id, "diagnostic").unwrap();
        match registry.execute(&account, &request, CancelToken::new()) {
            Ok(response) => {
                println!(
                    "{}: active={}, tabs={}, message={}",
                    args["action"],
                    response.active_tab_id,
                    response.tabs.len(),
                    response.message
                );
                if let Some(page) = response.page {
                    println!("page: url={}, title={}", page.url, page.title);
                    if let Some(shot) = page.screenshot {
                        let path = std::env::temp_dir().join(format!(
                            "anticrumb-browser-diagnostic-{}.png",
                            uuid::Uuid::new_v4().simple()
                        ));
                        std::fs::File::create_new(&path)
                            .unwrap()
                            .write_all(&shot.png)
                            .unwrap();
                        println!(
                            "screenshot: {}x{}, {} bytes, {}",
                            shot.width,
                            shot.height,
                            shot.png.len(),
                            path.display()
                        );
                    }
                }
            }
            Err(error) => {
                failed = true;
                println!("{}: error: {}", args["action"], error.message);
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
