use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
#[cfg(windows)]
fn databricks_oauth_is_unsupported_on_windows() {
    struct NoBrowser;
    impl BrowserOpener for NoBrowser {
        fn open(&self, _: &str) -> Result<(), String> {
            panic!("Unsupported sign-in opened a browser");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let connection = RuntimeFactory.open(
        "https://workspace.example.invalid",
        dir.path(),
        Arc::new(NoBrowser),
    );
    assert_eq!(
        connection.err().as_deref(),
        Some(buzz_agent_controller::connection::DATABRICKS_WINDOWS)
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[test]
#[cfg(unix)]
fn runtime_factory_no_ambient_auth_on_construction_or_empty_headless_refresh() {
    struct NoBrowser;
    impl BrowserOpener for NoBrowser {
        fn open(&self, _: &str) -> Result<(), String> {
            panic!("Passive browser launch");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    // No live endpoint can be contacted with an empty app-isolated token cache.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let connection = RuntimeFactory
            .open(
                "https://workspace.example.invalid",
                dir.path(),
                Arc::new(NoBrowser),
            )
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), connection.models(None))
            .await
            .unwrap();
        assert!(result.is_err());
        // Actual production connect forwarding cannot silently become a no-op.
        // Own the listener until it observes a TLS attempt, then close it rather
        // than depending on platform-specific refused-connection retry timing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connection = RuntimeFactory
            .open(
                &format!("https://127.0.0.1:{port}"),
                dir.path(),
                Arc::new(NoBrowser),
            )
            .unwrap();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(connection.connect(), async move {
                let (peer, _) = listener.accept().await.unwrap();
                let mut hello = [0u8; 1];
                assert_eq!(peer.peek(&mut hello).await.unwrap(), 1);
            })
        })
        .await
        .unwrap();
        assert!(result.is_err());
    });
    assert!(RuntimeFactory
        .open(
            "http://workspace.example.com",
            dir.path(),
            Arc::new(NoBrowser)
        )
        .is_err());
}
#[tokio::test]
async fn cancel_fences_begin_run_and_waits_for_drop() {
    let dir = tempfile::tempdir().unwrap();
    let host = ModelHost::new(Ok(dir.path().join("models")));
    let ticket = host.begin().unwrap();
    host.cancel(ticket).unwrap();
    assert!(host
        .run(ticket, async { panic!("Cancelled ticket ran") })
        .await
        .is_err());
    let ticket = host.begin().unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    struct DropGuard(Arc<AtomicUsize>);
    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let guard = DropGuard(dropped.clone());
    let owner = host.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let running = tokio::spawn(async move {
        owner
            .run(ticket, async move {
                let _guard = guard;
                let _ = started.send(());
                std::future::pending().await
            })
            .await
    });
    ready.await.unwrap();
    assert!(host.begin().is_err());
    host.cancel(ticket).unwrap();
    assert!(host.begin().is_err()); // cancellation is requested, not retirement
    assert!(tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("Cancellation did not drop actual native work")
        .unwrap()
        .is_err());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let next = host.begin().unwrap();
    host.cancel(ticket).unwrap(); // stale cancel cannot cancel next
    assert!(host.begin().is_err());
    host.cancel(next).unwrap();
    host.shutdown();
    assert!(host.begin().is_err());
}
#[test]
fn workspace_policy_and_cache_are_canonical_and_separate() {
    assert_eq!(
        origin("https://EXAMPLE.com:443/").unwrap(),
        "https://example.com"
    );
    for raw in [
        "",
        "http://example.com",
        "https://u:p@example.com",
        "https://example.com/path",
        "https://example.com?token=SECRET",
        "https://example.com#x",
        "https://example.com/../",
        " https://example.com",
        "https://example.com\\evil",
    ] {
        let error = origin(raw).unwrap_err();
        assert!(!error.contains("SECRET"));
    }
    let dir = tempfile::tempdir().unwrap();
    let a = ModelHost::new(Ok(dir.path().join("app-a")));
    let b = ModelHost::new(Ok(dir.path().join("app-b")));
    assert_ne!(
        a.cache("https://example.com").unwrap(),
        b.cache("https://example.com").unwrap()
    );
}

#[cfg(unix)]
#[test]
fn linked_cache_is_refused_before_credential_work() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let root = dir.path().join("models");
    std::os::unix::fs::symlink(other.path(), &root).unwrap();
    assert!(ModelHost::new(Ok(root))
        .cache("https://example.com")
        .is_err());
}

#[tokio::test]
async fn unstarted_ticket_expires_and_old_run_cannot_claim_its_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let host = ModelHost::new(Ok(dir.path().join("models")));
    let expired = host.begin().unwrap();
    host.state.lock().unwrap().pending.as_mut().unwrap().created =
        std::time::Instant::now() - Duration::from_secs(16);
    assert!(host
        .run(expired, async { panic!("Expired ticket ran") })
        .await
        .is_err());
    let next = host.begin().unwrap();
    assert_ne!(next, expired);
    assert!(host
        .run(expired, async { panic!("Stale ticket ran") })
        .await
        .is_err());
    host.cancel(next).unwrap();
    assert!(host.begin().is_ok());
}
