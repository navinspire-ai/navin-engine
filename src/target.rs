//! Work out what to run and where it answers.
//!
//! Every engine needs two facts: the command that starts the application
//! and the URL it serves. Both are questions about the project, not about
//! the operator, so the engine answers them itself and only asks when its
//! own investigation comes up empty.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::progress::ProgressSink;
use crate::project::{inspect_project, start_command, suggested_ports};
use crate::runner::discover::{discover_url, UrlCache};

/// Time given to an application to boot during discovery. Generous: a cold
/// bundler or a JVM is slow the first time.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(90);

/// How to reach the application under test.
#[derive(Debug, Clone)]
pub struct Target {
    pub start_cmd: String,
    pub url: String,
    /// True when the engine had to boot the app to find its port.
    pub discovered: bool,
}

/// Fill in whatever the caller did not provide.
///
/// The start command comes from the project manifest. The URL, which no
/// static analysis can know for sure, is observed: the app is booted once
/// in a throwaway shadow, watched until it opens a port, then stopped.
pub async fn resolve(
    root: &Path,
    start: Option<String>,
    url: Option<String>,
    sink: &dyn ProgressSink,
) -> Result<Target> {
    let manifest = inspect_project(root)?;
    let start_cmd = match start {
        Some(given) => given,
        None => start_command(root, &manifest).with_context(|| {
            format!(
                "cannot tell how to start this project: nothing found in {}, \
                 give a start command",
                root.display()
            )
        })?,
    };
    if let Some(url) = url {
        return Ok(Target { start_cmd, url, discovered: false });
    }

    let cache = UrlCache::new(root);
    if let Some(url) = cache.get(&start_cmd) {
        sink.emit("target", "resolved", serde_json::json!({ "url": url, "source": "cache" }));
        return Ok(Target { start_cmd, url, discovered: false });
    }

    sink.emit("target", "discovering", serde_json::json!({ "start": start_cmd }));
    let manager = crate::shadow::ShadowManager::new(root);
    let guard = crate::shadow::cleanup::CleanupGuard::new(manager.create("discover")?);
    let log = crate::engine_dir(root).join("logs").join("discover.log");
    let url = discover_url(
        &start_cmd,
        guard.path(),
        &log,
        &suggested_ports(&manifest),
        DISCOVERY_TIMEOUT,
        None,
    )
    .await;
    guard.destroy()?;
    let url = url?;

    cache.put(&start_cmd, &url).ok();
    sink.emit("target", "resolved", serde_json::json!({ "url": url, "source": "probe" }));
    Ok(Target { start_cmd, url, discovered: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::RecordingSink;

    #[tokio::test]
    async fn a_given_target_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let target = resolve(
            tmp.path(),
            Some("node server.js".to_owned()),
            Some("http://127.0.0.1:9999/".to_owned()),
            &RecordingSink::default(),
        )
        .await
        .unwrap();
        assert_eq!(target.start_cmd, "node server.js");
        assert!(!target.discovered);
    }

    #[tokio::test]
    async fn a_project_with_no_entry_point_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("README.md"), "nothing runnable here").unwrap();
        let err = resolve(tmp.path(), None, None, &RecordingSink::default()).await.unwrap_err();
        assert!(format!("{err:#}").contains("cannot tell how to start"));
    }

    #[tokio::test]
    async fn the_url_is_learnt_by_booting_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let port = crate::runner::ports::free_port().unwrap();
        std::fs::write(tmp.path().join("Procfile"), format!("web: python3 -m http.server {port}\n"))
            .unwrap();
        let target = resolve(tmp.path(), None, None, &RecordingSink::default()).await.unwrap();
        assert_eq!(target.url, format!("http://127.0.0.1:{port}/"));
        assert!(target.discovered);
        // The answer is remembered: the second call must not boot anything.
        let again = resolve(tmp.path(), None, None, &RecordingSink::default()).await.unwrap();
        assert_eq!(again.url, target.url);
        assert!(!again.discovered);
    }
}
