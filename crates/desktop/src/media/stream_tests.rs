use super::*;
use crate::media::tests::{
    hung_up, media_blob, media_response, ranged_media_server, spool_files_deleted,
    spool_files_reach, whole_media_server, SPOOL_TESTS,
};
use crate::media::{blocks::BLOCK, spool as media_spool};
use crate::test_support::TestSigner;
use std::io::{Read as _, Write as _};

/// Any URL, so the fixture origins may be plain loopback HTTP.
fn any_url(target: &str) -> Option<Url> {
    Url::parse(target).ok()
}

fn stream() -> MediaStream {
    MediaStream::serve(TestSigner, any_url, &tokio::runtime::Handle::current()).unwrap()
}

/// The listener's authority and the request target for `url`.
fn target(stream: &MediaStream, url: &Url) -> (String, String) {
    let base = Url::parse(stream.base()).unwrap();
    let authority = format!("127.0.0.1:{}", base.port().unwrap());
    let encoded =
        percent_encoding::utf8_percent_encode(url.as_str(), percent_encoding::NON_ALPHANUMERIC);
    (authority, format!("{}{encoded}", base.path()))
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Writes `head` and reads the reply until the listener closes.
async fn exchange(authority: &str, head: &str) -> Reply {
    let mut socket = TcpStream::connect(authority).await.unwrap();
    socket.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).await.unwrap();
    parse_reply(&bytes)
}

fn parse_reply(bytes: &[u8]) -> Reply {
    let end = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("a complete response head");
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(": ").unwrap();
            (name.to_owned(), value.to_owned())
        })
        .collect();
    Reply {
        status,
        headers,
        body: bytes[end + 4..].to_vec(),
    }
}

async fn get(stream: &MediaStream, url: &Url, extra: &str) -> Reply {
    let (authority, path) = target(stream, url);
    exchange(
        &authority,
        &format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n"),
    )
    .await
}

fn length(reply: &Reply) -> usize {
    reply.header("content-length").unwrap().parse().unwrap()
}

#[tokio::test]
async fn requests_need_the_token_the_bound_host_and_the_app_origin() {
    let blob = media_blob(64);
    let (origin, _) = ranged_media_server(blob.clone(), 0);
    let url = origin
        .join(&format!("/media/{}.mp4", "1".repeat(64)))
        .unwrap();
    let stream = stream();
    let (authority, path) = target(&stream, &url);
    let wrong_token = path.replacen('/', "/x", 1);
    let cases = [
        (format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n"), 200),
        (
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nOrigin: tauri://localhost\r\n\r\n"),
            200,
        ),
        (
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://tauri.localhost\r\n\r\n"),
            200,
        ),
        (format!("GET {wrong_token} HTTP/1.1\r\nHost: {authority}\r\n\r\n"), 404),
        (format!("GET / HTTP/1.1\r\nHost: {authority}\r\n\r\n"), 404),
        // DNS rebinding names another host.
        (format!("GET {path} HTTP/1.1\r\nHost: media.example\r\n\r\n"), 403),
        (format!("GET {path} HTTP/1.1\r\n\r\n"), 403),
        (
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nHost: {authority}\r\n\r\n"),
            400,
        ),
        (
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nOrigin: https://site.example\r\n\r\n"),
            403,
        ),
        (format!("POST {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n"), 405),
        (
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nTransfer-Encoding: chunked\r\n\r\n"),
            400,
        ),
        (format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\n\r\n"), 400),
    ];
    for (head, status) in cases {
        let reply = exchange(&authority, &head).await;
        assert_eq!(reply.status, status, "{head}");
        if status == 200 {
            assert_eq!(reply.body, blob);
        } else {
            assert!(reply.body.is_empty());
        }
    }
    // The real validator still applies: never an arbitrary URL.
    let strict =
        MediaStream::serve(TestSigner, media_url, &tokio::runtime::Handle::current()).unwrap();
    assert_eq!(get(&strict, &url, "").await.status, 403);
}

/// `HEAD_LIMIT` bounds the whole head, its terminating blank line included.
#[tokio::test]
async fn a_request_head_may_fill_but_not_exceed_the_head_limit() {
    let blob = media_blob(64);
    let (origin, _) = ranged_media_server(blob.clone(), 0);
    let url = origin
        .join(&format!("/media/{}.mp4", "1".repeat(64)))
        .unwrap();
    let stream = stream();
    let (authority, path) = target(&stream, &url);
    let head = |size: usize| {
        let start = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nX-Pad: ");
        let pad = "a".repeat(size - start.len() - 4);
        format!("{start}{pad}\r\n\r\n")
    };
    let fits = head(HEAD_LIMIT);
    assert_eq!(fits.len(), HEAD_LIMIT);
    let reply = exchange(&authority, &fits).await;
    assert_eq!((reply.status, reply.body), (200, blob));

    let over = head(HEAD_LIMIT + 1);
    let mut socket = TcpStream::connect(&authority).await.unwrap();
    let _ = socket.write_all(over.as_bytes()).await;
    let mut bytes = Vec::new();
    // Closed without a response; unread bytes may turn the close into a reset.
    let _ = socket.read_to_end(&mut bytes).await;
    assert!(bytes.is_empty());
}

/// Every response shape, for an origin that answers `Range` and for one that
/// ignores it and is spooled.
#[tokio::test]
async fn full_head_suffix_and_open_ended_responses_have_exact_boundaries() {
    let _serial = SPOOL_TESTS.lock().await;
    let total = 2 * BLOCK as usize + 100;
    let blob = media_blob(total as u64);
    let (ranged, _) = ranged_media_server(blob.clone(), 0);
    let (whole, _, _) = whole_media_server(blob.clone(), Some(total as u64), Duration::ZERO);
    let stream = stream();
    for (origin, name) in [(ranged, "2"), (whole, "3")] {
        let url = origin
            .join(&format!("/media/{}.mp4", name.repeat(64)))
            .unwrap();
        let full = get(&stream, &url, "").await;
        assert_eq!(full.status, 200);
        assert_eq!(length(&full), total);
        assert_eq!(full.body, blob);
        assert_eq!(full.header("content-type"), Some("video/mp4"));
        assert_eq!(full.header("accept-ranges"), Some("bytes"));
        assert_eq!(full.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(full.header("content-range"), None);

        let (authority, path) = target(&stream, &url);
        let head = exchange(
            &authority,
            &format!("HEAD {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
        )
        .await;
        assert_eq!(head.status, 200);
        assert_eq!(length(&head), total);
        assert!(head.body.is_empty());

        let cases: [(&str, usize, usize); 6] = [
            ("bytes=-10", total - 10, total - 1),
            ("bytes=-999999999", 0, total - 1),
            ("bytes=100-", 100, total - 1),
            ("bytes=5-9", 5, 9),
            // Across a block boundary, and past the end.
            ("bytes=1048570-1048580", 1_048_570, 1_048_580),
            ("bytes=2097200-9999999", 2_097_200, total - 1),
        ];
        for (range, start, end) in cases {
            let reply = get(&stream, &url, &format!("Range: {range}\r\n")).await;
            assert_eq!(reply.status, 206, "{range}");
            assert_eq!(
                reply.header("content-range").unwrap(),
                format!("bytes {start}-{end}/{total}"),
                "{range}"
            );
            assert_eq!(length(&reply), end - start + 1, "{range}");
            assert_eq!(reply.body, &blob[start..=end], "{range}");
        }
        let head = exchange(
            &authority,
            &format!("HEAD {path} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes=-10\r\n\r\n"),
        )
        .await;
        assert_eq!(head.status, 206);
        assert_eq!(length(&head), 10);
        assert!(head.body.is_empty());
        for range in [format!("bytes={total}-"), "bytes=-0".to_owned()] {
            let reply = get(&stream, &url, &format!("Range: {range}\r\n")).await;
            assert_eq!(reply.status, 416, "{range}");
            assert!(reply.body.is_empty());
        }
    }
    spool_files_deleted().await;
}

/// Serves the first block by `Range`, then fails every later request.
fn failing_media_server(blob: Vec<u8>) -> Url {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    std::thread::spawn(move || {
        for (index, socket) in listener.incoming().enumerate() {
            let mut socket = socket.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..count]);
            }
            let response = if index == 0 {
                let body = &blob[..BLOCK as usize];
                let range = format!(
                    "Content-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\n",
                    BLOCK - 1,
                    blob.len(),
                    body.len()
                );
                media_response("206 Partial Content", &range, body)
            } else {
                media_response("503 Service Unavailable", "Content-Length: 0\r\n", &[])
            };
            let _ = socket.write_all(&response);
        }
    });
    base
}

#[tokio::test]
async fn a_failure_mid_stream_ends_the_response_short_of_its_length() {
    let _serial = SPOOL_TESTS.lock().await;
    let total = 3 * BLOCK;
    let blob = media_blob(total);
    let stream = stream();
    // A range origin failing after the first block.
    let url = failing_media_server(blob.clone())
        .join(&format!("/media/{}.mp4", "4".repeat(64)))
        .unwrap();
    let reply = get(&stream, &url, "").await;
    assert_eq!(reply.status, 200);
    assert_eq!(length(&reply), total as usize);
    assert_eq!(reply.body, &blob[..BLOCK as usize]);
    // A whole 200 shorter than its `Content-Length`.
    let (whole, _, _) = whole_media_server(
        blob[..2 * BLOCK as usize].to_vec(),
        Some(total),
        Duration::ZERO,
    );
    let url = whole
        .join(&format!("/media/{}.mp4", "5".repeat(64)))
        .unwrap();
    let reply = get(&stream, &url, "").await;
    assert_eq!(reply.status, 200);
    assert_eq!(length(&reply), total as usize);
    assert!(reply.body.len() < total as usize);
    assert_eq!(reply.body, &blob[..reply.body.len()]);
    spool_files_deleted().await;
}

/// Sends a request and keeps the connection, reading at most `read` bytes of
/// the reply, until dropped.
async fn open(stream: &MediaStream, url: &Url, extra: &str, read: usize) -> TcpStream {
    let (authority, path) = target(stream, url);
    let mut socket = TcpStream::connect(&authority).await.unwrap();
    socket
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n").as_bytes())
        .await
        .unwrap();
    let mut bytes = vec![0; read];
    socket.read_exact(&mut bytes).await.unwrap();
    socket
}

#[tokio::test]
async fn closing_while_waiting_for_an_unknown_length_stops_the_download() {
    let _serial = SPOOL_TESTS.lock().await;
    // Over ten seconds to copy, longer than `spool_files_deleted` waits.
    let blob = media_blob(32 * BLOCK);
    let (origin, _, hangups) = whole_media_server(blob, None, Duration::from_millis(20));
    let url = origin
        .join(&format!("/media/{}.mp4", "6".repeat(64)))
        .unwrap();
    let stream = stream();
    // Nothing can be answered before the copy ends: not even a head.
    let socket = open(&stream, &url, "", 0).await;
    spool_files_reach(1).await;
    drop(socket);
    spool_files_deleted().await;
    hung_up(&hangups, 1).await;
}

#[tokio::test]
async fn closing_after_the_head_or_during_a_seek_stops_the_download() {
    let _serial = SPOOL_TESTS.lock().await;
    let total = 32 * BLOCK;
    let blob = media_blob(total);
    let (origin, _, hangups) = whole_media_server(blob, Some(total), Duration::from_millis(20));
    let stream = stream();
    let url = origin
        .join(&format!("/media/{}.mp4", "7".repeat(64)))
        .unwrap();
    // The head and a few bytes arrive, then the player closes.
    let socket = open(&stream, &url, "", 64).await;
    assert_eq!(media_spool::open_files(), 1);
    drop(socket);
    spool_files_deleted().await;
    hung_up(&hangups, 1).await;

    let url = origin
        .join(&format!("/media/{}.mp4", "8".repeat(64)))
        .unwrap();
    let first = get(&stream, &url, "Range: bytes=0-7\r\n").await;
    assert_eq!(first.status, 206);
    // A far seek waits for bytes, then the player closes.
    let seek = open(
        &stream,
        &url,
        &format!("Range: bytes={}-\r\n", total - 8),
        0,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(seek);
    spool_files_deleted().await;
    hung_up(&hangups, 2).await;
}

#[tokio::test]
async fn another_players_connection_keeps_a_shared_spool() {
    let _serial = SPOOL_TESTS.lock().await;
    let total = 32 * BLOCK;
    let blob = media_blob(total);
    let (origin, served, hangups) =
        whole_media_server(blob, Some(total), Duration::from_millis(20));
    let url = origin
        .join(&format!("/media/{}.mp4", "9".repeat(64)))
        .unwrap();
    let stream = stream();
    let staying = open(&stream, &url, "", 64).await;
    let leaving = open(&stream, &url, "Range: bytes=0-\r\n", 64).await;
    drop(leaving);
    // Well past the idle time, the remaining player still holds the spool.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(media_spool::open_files(), 1);
    assert_eq!(hangups.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(staying);
    spool_files_deleted().await;
    hung_up(&hangups, 1).await;
}

#[tokio::test]
async fn connections_over_the_limit_wait_to_be_accepted() {
    let blob = media_blob(64);
    let (origin, _) = ranged_media_server(blob.clone(), 0);
    let url = origin
        .join(&format!("/media/{}.mp4", "a".repeat(64)))
        .unwrap();
    let stream = stream();
    let (authority, path) = target(&stream, &url);
    let mut silent = Vec::new();
    for _ in 0..CONNECTIONS {
        silent.push(TcpStream::connect(&authority).await.unwrap());
    }
    // Let the listener accept every silent connection first.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let waiting = tokio::spawn({
        let authority = authority.clone();
        async move {
            exchange(
                &authority,
                &format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished());
    drop(silent.pop());
    let reply = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.body, blob);
}

#[tokio::test]
async fn a_silent_connection_is_closed_after_the_head_timeout() {
    let stream = stream();
    let base = Url::parse(stream.base()).unwrap();
    let mut socket = TcpStream::connect(("127.0.0.1", base.port().unwrap()))
        .await
        .unwrap();
    socket.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    let mut bytes = Vec::new();
    let read = tokio::time::timeout(
        HEAD_TIMEOUT + Duration::from_secs(1),
        socket.read_to_end(&mut bytes),
    )
    .await;
    assert!(read.is_ok());
    assert!(bytes.is_empty());
}

#[test]
fn byte_ranges_are_single_and_well_formed() {
    assert_eq!(byte_range("bytes=0-"), Some(Range::From(0)));
    assert_eq!(byte_range("bytes=3-9"), Some(Range::Between(3, 9)));
    assert_eq!(byte_range("bytes=-5"), Some(Range::Suffix(5)));
    for ignored in [
        "bytes=9-3",
        "bytes=0-1,4-5",
        "items=0-1",
        "bytes=-",
        "bytes=+1-2",
        "bytes= 1-2",
    ] {
        assert_eq!(byte_range(ignored), None, "{ignored}");
    }
}
