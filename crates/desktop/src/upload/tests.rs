use super::*;
use crate::test_support::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;

async fn upload(
    host: &TestSigner,
    url: Url,
    kind: Option<&str>,
    bytes: Vec<u8>,
    progress: impl Fn(UploadSent) + Send + Sync + 'static,
) -> Result<RelayResponse> {
    validate_upload_size(bytes.len())?;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source");
    std::fs::write(&path, bytes).unwrap();
    let hash = spool::hash_file(&path)?;
    upload_file(host, url, kind, path, hash, progress).await
}

#[tokio::test]
async fn upload_signs_the_exact_bytes_it_sends() {
    let (base, task) = fixture_server(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into(),
    );
    let url = base.join("/upload").unwrap();
    // ASCII: the fixture server compares lengths on decoded text.
    let body = b"PNG fixture bytes".to_vec();
    let hash = format!("{:x}", Sha256::digest(&body));
    let result = upload(
        &TestSigner,
        url.clone(),
        Some("image/png"),
        body.clone(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!((result.status, result.body.as_str()), (200, "{}"));
    let (headers, sent) = task.join().unwrap();
    assert_eq!(sent.as_bytes(), body);
    assert!(headers.starts_with("PUT /upload "));
    assert!(headers
        .lines()
        .any(|line| line == format!("x-sha-256: {hash}")));
    assert!(headers
        .lines()
        .any(|line| line == "content-type: image/png"));
    let event = blossom_event(&headers);
    let server = &url[url::Position::BeforeHost..url::Position::AfterPort];
    assert_strict(&event, "upload", server);
    assert_eq!(tag(&event, "x"), [hash.as_str()]);
    assert!(upload(&TestSigner, url, None, Vec::new(), |_| {})
        .await
        .is_err());
}

#[tokio::test]
async fn upload_reports_bytes_handed_to_the_connection() {
    let (base, task) = fixture_server(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into(),
    );
    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = reports.clone();
    // Four 64 KiB chunks, ASCII for the fixture server's text comparison.
    let body = vec![b'a'; 3 * UPLOAD_CHUNK + 1];
    let result = upload(
        &TestSigner,
        base.join("/upload").unwrap(),
        Some("image/png"),
        body.clone(),
        move |sent| sink.lock().unwrap().push(sent),
    )
    .await
    .unwrap();
    assert_eq!(result.status, 200);
    let (headers, sent) = task.join().unwrap();
    assert_eq!(sent.as_bytes(), body);
    assert!(headers
        .lines()
        .any(|line| line == format!("content-length: {}", body.len())));
    let total = body.len();
    assert_eq!(
        *reports.lock().unwrap(),
        [UPLOAD_CHUNK, 2 * UPLOAD_CHUNK, 3 * UPLOAD_CHUNK, total].map(|sent| UploadSent {
            sent: sent as u64,
            total: total as u64
        })
    );
}

#[tokio::test]
async fn single_chunk_upload_reports_its_whole_body() {
    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = reports.clone();
    use futures_util::StreamExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source");
    std::fs::write(&path, vec![0; 5263]).unwrap();
    let file = std::fs::File::open(path).unwrap();
    let chunks = file_chunks(file, 5263, move |sent| sink.lock().unwrap().push(sent));
    assert_eq!(chunks.collect::<Vec<_>>().await.len(), 1);
    assert_eq!(
        *reports.lock().unwrap(),
        [UploadSent {
            sent: 5263,
            total: 5263
        }]
    );
}

#[tokio::test]
async fn progress_reports_change_by_whole_percent_only() {
    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = reports.clone();
    use futures_util::StreamExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len((1000 * UPLOAD_CHUNK) as u64).unwrap();
    drop(file);
    let file = std::fs::File::open(path).unwrap();
    let chunks = file_chunks(file, (1000 * UPLOAD_CHUNK) as u64, move |sent| {
        sink.lock().unwrap().push(sent.sent)
    });
    assert_eq!(chunks.collect::<Vec<_>>().await.len(), 1000);
    let reports = reports.lock().unwrap();
    // Percent 0 (first nine chunks) through 100, once each.
    assert_eq!(reports.len(), 101);
    assert!(reports.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(reports.last(), Some(&(1000 * UPLOAD_CHUNK as u64)));
}

#[test]
fn uploads_cancel_before_or_during_and_reject_duplicates() {
    let uploads = Uploads::default();
    let mut running = uploads.start("a").unwrap().unwrap();
    assert!(uploads.start("a").is_err());
    uploads.cancel("a");
    assert!(running.try_recv().is_ok());
    uploads.finish("a");
    // A cancel that overtakes its upload stops it from starting, once.
    uploads.cancel("b");
    assert!(uploads.start("b").unwrap().is_none());
    assert!(uploads.start("b").unwrap().is_some());
    for id in ["", "a/b", &"x".repeat(65)] {
        assert!(upload_id(Some(id)).is_err(), "{id}");
    }
    assert!(upload_id(None).is_err());
}

#[test]
fn late_cancels_cannot_exhaust_upload_admission() {
    let uploads = Uploads::default();
    for n in 0..128 {
        let id = n.to_string();
        let _running = uploads.start(&id).unwrap().unwrap();
        uploads.finish(&id);
        uploads.cancel(&id); // Renderer received completion after native finished.
    }
    assert!(uploads.start("fresh").unwrap().is_some());
    assert_eq!(uploads.lock().pending.len(), 64);
    // Early rejection before `start` has the same late-cancel path.
    uploads.cancel("rejected-before-start");
    assert!(uploads.start("another").unwrap().is_some());
    // Even when active admission is full, a pre-cancelled ID never starts.
    let mut held = Vec::new();
    for n in 0..62 {
        held.push(uploads.start(&format!("active-{n}")).unwrap().unwrap());
    }
    uploads.cancel("queued");
    assert!(uploads.start("queued").unwrap().is_none());
    assert!(uploads.start("overflow").is_err());
    uploads.finish("active-0");
    assert!(uploads.start("overflow").unwrap().is_some());
    uploads.finish("active-1");
    let mut active = uploads.start("active").unwrap().unwrap();
    uploads.cancel("active");
    assert!(active.try_recv().is_ok());
}

#[test]
fn upload_hash_reads_exact_bytes_from_disk() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source");
    std::fs::write(&path, b"upload bytes").unwrap();
    let (size, hash) = spool::hash_file(&path).unwrap();
    assert_eq!(size, 12);
    assert_eq!(hash, format!("{:x}", Sha256::digest(b"upload bytes")));
    std::fs::write(&path, []).unwrap();
    assert!(spool::hash_file(&path).is_err());
    assert!(validate_upload_size(0).is_err());
    assert!(validate_upload_size(MAX_UPLOAD + 1).is_err());
    assert!(validate_upload_size(MAX_UPLOAD).is_ok());
}

#[tokio::test]
async fn spool_processing_cleans_after_upstream_and_conversion_failures() {
    for preparation in [None, Some("video:avi")] {
        let parent = tempfile::tempdir().unwrap();
        let spools = Spools::new(Ok(parent.path().to_owned()));
        let uploads = Uploads::default();
        spools.begin(&uploads, "failure", 3).unwrap();
        spools.append("failure", 0, b"bad").unwrap();
        let (spool, mut cancel) = spools.take("failure").unwrap();
        let path = spool.directory.path().to_owned();
        // Closed loopback port for upstream failure; invalid AVI for conversion.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/upload", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let result = process_spool(
            &TestSigner,
            url,
            spool,
            None,
            preparation,
            |_| {},
            &mut cancel,
        )
        .await;
        if preparation.is_some() {
            assert!(result.unwrap().status >= 400);
        } else {
            assert!(result.is_err());
        }
        assert!(!path.exists());
        assert!(spools.begin(&uploads, "next", 3).is_ok());
    }
}

#[tokio::test]
async fn cancelled_spool_processing_never_starts_upstream_and_cleans() {
    let parent = tempfile::tempdir().unwrap();
    let spools = Spools::new(Ok(parent.path().to_owned()));
    let uploads = Uploads::default();
    spools.begin(&uploads, "cancel", 3).unwrap();
    spools.append("cancel", 0, b"abc").unwrap();
    let (spool, mut cancel) = spools.take("cancel").unwrap();
    let path = spool.directory.path().to_owned();
    uploads.cancel("cancel");
    assert!(process_spool(
        &TestSigner,
        Url::parse("https://relay.test/upload").unwrap(),
        spool,
        None,
        None,
        |_| {},
        &mut cancel
    )
    .await
    .is_err());
    assert!(!path.exists());
}

#[tokio::test]
async fn converted_spool_streams_signed_output_then_removes_both_files() {
    let program = crate::process::resolve_program("ffmpeg", &crate::process::effective_path());
    if !program.exists() {
        eprintln!("ffmpeg unavailable: converted-spool integration not executed");
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let input = parent.path().join("input.avi");
    assert!(std::process::Command::new(program)
        .args([
            "-y",
            "-nostdin",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=16x16:r=1",
            "-t",
            "1",
            "-c:v",
            "mpeg4"
        ])
        .arg(&input)
        .status()
        .unwrap()
        .success());
    let bytes = std::fs::read(&input).unwrap();
    let spools = Spools::new(Ok(parent.path().join("spools")));
    let uploads = Uploads::default();
    spools.begin(&uploads, "converted", bytes.len()).unwrap();
    for (index, chunk) in bytes.chunks(UPLOAD_CHUNK).enumerate() {
        spools
            .append("converted", index * UPLOAD_CHUNK, chunk)
            .unwrap();
    }
    let (spool, mut cancel) = spools.take("converted").unwrap();
    let path = spool.directory.path().to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/upload", listener.local_addr().unwrap())).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut block = [0; 4096];
        loop {
            let read = socket.read(&mut block).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&block[..read]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let header = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let size: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() == end + 4 + size {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                    return (header, bytes[end + 4..].to_vec());
                }
            }
        }
    });
    let response = process_spool(
        &TestSigner,
        url,
        spool,
        None,
        Some("video:avi"),
        |_| {},
        &mut cancel,
    )
    .await
    .unwrap();
    assert_eq!(response.status, 200);
    let (header, body) = server.join().unwrap();
    assert!(body.windows(4).any(|part| part == b"ftyp"));
    assert_ne!(body, bytes);
    assert!(header.lines().any(|line| line == "content-type: video/mp4"));
    let hash = format!("{:x}", Sha256::digest(&body));
    assert_eq!(tag(&blossom_event(&header), "x"), [hash.as_str()]);
    assert!(!path.exists());
}

#[tokio::test]
async fn running_upstream_cancellation_closes_request_and_cleans_spool() {
    let parent = tempfile::tempdir().unwrap();
    let spools = Spools::new(Ok(parent.path().to_owned()));
    let uploads = Uploads::default();
    spools.begin(&uploads, "running", 3).unwrap();
    spools.append("running", 0, b"abc").unwrap();
    let (spool, mut cancel) = spools.take("running").unwrap();
    let path = spool.directory.path().to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/upload", listener.local_addr().unwrap())).unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut block = [0; 4096];
        loop {
            let read = socket.read(&mut block).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&block[..read]);
            if bytes.ends_with(b"abc") {
                break;
            }
        }
        entered.send(()).unwrap();
        // Stall the response, not the scheduler: cancellation must close it.
        socket.read(&mut block).unwrap()
    });
    let host = TestSigner;
    let work = process_spool(&host, url, spool, None, None, |_| {}, &mut cancel);
    tokio::pin!(work);
    tokio::select! {
        _ = ready => uploads.cancel("running"),
        _ = &mut work => panic!("upload settled before fixture gate"),
    }
    assert_eq!(work.await.err().unwrap(), "Upload cancelled");
    assert_eq!(
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap(),
        0
    );
    assert!(!path.exists());
}
