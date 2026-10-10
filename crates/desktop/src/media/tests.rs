use super::*;
use super::{blocks as media_blocks, spool as media_spool};
use crate::test_support::*;
use std::io::{Read, Write};
use std::net::TcpListener;

#[test]
fn media_proxy_only_reaches_relay_blobs() {
    let hash = "a".repeat(64);
    for target in [
        format!("https://relay.test/media/{hash}"),
        format!("https://relay.test/media/{hash}.png"),
        format!("https://relay.test:8443/media/{hash}.thumb.jpg"),
    ] {
        assert!(media_url(&target).is_some(), "{target}");
    }
    for target in [
        format!("http://relay.test/media/{hash}"),
        format!("https://u:p@relay.test/media/{hash}"),
        format!("https://relay.test/media/{hash}?x=1"),
        format!("https://relay.test/media/{hash}#x"),
        format!("https://relay.test/media/{hash}?"),
        format!("https://relay.test/media/{hash}#"),
        format!("https://relay.test/media/{hash}.png#buzz-anim=https%3A%2F%2Frelay.test%2Fmedia%2F{hash}.png"),
        format!("https://relay.test/media/{hash}.tar.gz"),
        format!("https://relay.test/upload/{hash}"),
        format!("https://relay.test/media/{}", "A".repeat(64)),
        format!("https://relay.test/media/{hash}/../../query"),
        format!("https://relay.test/media/{hash}.PNG"),
        "https://relay.test/media/abc".into(),
        "file:///etc/passwd".into(),
    ] {
        assert!(media_url(&target).is_none(), "{target}");
    }
}

#[test]
fn native_downloads_only_accept_authenticated_media_urls() {
    let hash = "a".repeat(64);
    let target = format!("https://relay.test/media/{hash}.pdf");
    let encoded: String =
        percent_encoding::utf8_percent_encode(&target, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    for url in [
        format!("buzz-media://localhost/{encoded}"),
        format!("http://buzz-media.localhost/{encoded}"),
    ] {
        assert!(download_target(&url).is_some(), "{url}");
    }
    for url in [
        format!("buzz-media://evil.test/{encoded}"),
        format!("http://buzz-media.localhost.evil.test/{encoded}"),
        format!("https://buzz-media.localhost/{encoded}"),
        format!("buzz-media://localhost:123/{encoded}"),
        format!("buzz-media://localhost/{encoded}?q=1"),
        format!("buzz-media://localhost/{encoded}#fragment"),
        format!("buzz-media://u:p@localhost/{encoded}"),
        format!(
            "buzz-media://localhost/{}",
            percent_encoding::utf8_percent_encode(
                "https://relay.test/query",
                percent_encoding::NON_ALPHANUMERIC
            )
        ),
    ] {
        assert!(download_target(&url).is_none(), "{url}");
    }
}

#[test]
fn clipboard_decodes_pixels_and_rejects_invalid_or_oversized_images() {
    use image::{ImageEncoder as _, Rgba, RgbaImage};
    let pixels = RgbaImage::from_fn(2, 1, |x, _| {
        if x == 0 {
            Rgba([255, 0, 0, 255])
        } else {
            Rgba([0, 80, 200, 128])
        }
    });
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(pixels.as_raw(), 2, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    assert_eq!(
        clipboard_pixels(&png).unwrap(),
        (2, 1, pixels.clone().into_raw())
    );
    let mut webp = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
        .write_image(pixels.as_raw(), 2, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    assert_eq!(clipboard_pixels(&webp).unwrap(), (2, 1, pixels.into_raw()));
    assert!(clipboard_pixels(b"not an image").is_err());
    assert!(clipboard_pixels(&vec![0; 50 * 1024 * 1024 + 1]).is_err());

    // A valid, compressible image exceeds the 50 MiB expanded RGBA cap.
    let huge = RgbaImage::from_pixel(4096, 4096, Rgba([1, 2, 3, 255]));
    let mut encoded = Vec::new();
    image::codecs::png::PngEncoder::new(&mut encoded)
        .write_image(huge.as_raw(), 4096, 4096, image::ExtendedColorType::Rgba8)
        .unwrap();
    assert!(encoded.len() < 50 * 1024 * 1024);
    assert!(clipboard_pixels(&encoded).is_err());
}

#[test]
fn clipboard_bounds_embedded_webp_vp8_frames_before_decode() {
    fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = tag.to_vec();
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() & 1 != 0 {
            bytes.push(0);
        }
        bytes
    }
    fn webp(chunks: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(chunks.len() as u32 + 4).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(chunks);
        bytes
    }
    let vp8 = |width: u16, height: u16| {
        let mut header = [0, 0, 0, 0x9d, 0x01, 0x2a, 0, 0, 0, 0];
        header[6..8].copy_from_slice(&width.to_le_bytes());
        header[8..10].copy_from_slice(&height.to_le_bytes());
        chunk(b"VP8 ", &header)
    };
    let mut static_chunks = chunk(b"VP8X", &[0, 0, 0, 0, 63, 0, 0, 63, 0, 0]);
    static_chunks.extend(vp8(64, 64));
    assert!(check_webp_vp8_frames(&webp(&static_chunks), 64 * 64 * 4).is_ok());
    assert!(check_webp_vp8_frames(&webp(&static_chunks), 4 * 4 * 384 - 1).is_err());
    let mut mismatch = chunk(b"VP8X", &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    mismatch.extend(vp8(64, 64));
    assert!(check_webp_vp8_frames(&webp(&mismatch), 64 * 64 * 4).is_err());

    // Padded VP8 planes have their own bound; odd dimensions must not lower
    // the existing (unpadded) RGBA output cap.
    let mut odd = chunk(b"VP8X", &[0, 0, 0, 0, 32, 0, 0, 32, 0, 0]);
    odd.extend(vp8(33, 33));
    assert!(check_webp_vp8_frames(&webp(&odd), 33 * 33 * 4).is_ok());
    assert!(check_webp_vp8_frames(&webp(&odd), 9 * 384 - 1).is_err());

    // The first animated frame nests VP8 after a 16-byte ANMF header.
    let mut frame = vec![0; 16];
    frame[6] = 63;
    frame[9] = 63;
    frame.extend(vp8(64, 64));
    let mut animated_chunks = chunk(b"VP8X", &[2, 0, 0, 0, 63, 0, 0, 63, 0, 0]);
    animated_chunks.extend(chunk(b"ANMF", &frame));
    assert!(check_webp_vp8_frames(&webp(&animated_chunks), 4 * 4 * 384 - 1).is_err());
    assert!(check_webp_vp8_frames(&webp(&animated_chunks), 64 * 64 * 4).is_ok());
    frame[6] = 0;
    assert!(check_webp_vp8_frames(&webp(&chunk(b"ANMF", &frame)), 64 * 64 * 4).is_err());
    frame[6] = 63;
    frame.pop();
    assert!(check_webp_vp8_frames(&webp(&chunk(b"ANMF", &frame)), 64 * 64 * 4).is_err());

    let mut alpha_frame = vec![0; 16];
    alpha_frame[6] = 63;
    alpha_frame[9] = 63;
    alpha_frame.extend(chunk(b"ALPH", &[0]));
    alpha_frame.extend(vp8(64, 64));
    assert!(check_webp_vp8_frames(&webp(&chunk(b"ANMF", &alpha_frame)), 64 * 64 * 4).is_ok());
    let vp8_tag = alpha_frame
        .windows(4)
        .position(|bytes| bytes == b"VP8 ")
        .unwrap();
    alpha_frame[vp8_tag..vp8_tag + 4].copy_from_slice(b"JUNK");
    assert!(check_webp_vp8_frames(&webp(&chunk(b"ANMF", &alpha_frame)), 64 * 64 * 4).is_err());
    alpha_frame.truncate(vp8_tag);
    assert!(check_webp_vp8_frames(&webp(&chunk(b"ANMF", &alpha_frame)), 64 * 64 * 4).is_err());

    let mut trailing = webp(&static_chunks);
    trailing.extend(vp8(64, 64));
    assert!(check_webp_vp8_frames(&trailing, 64 * 64 * 4).is_err());
}

#[test]
fn download_names_are_safe_and_collisions_do_not_overwrite() {
    let url = Url::parse(&format!("https://relay.test/media/{}.pdf", "a".repeat(64))).unwrap();
    for invalid in [
        "",
        ".",
        "..",
        "../secret",
        "a/b",
        "a\\b",
        "a:b",
        "a\n.txt",
        "a?.pdf",
        "a*.pdf",
        "a\".pdf",
        "a<.pdf",
        "a>.pdf",
        "a|.pdf",
        "CON",
        "con.txt",
        "NUL.pdf",
        "COM1.txt",
        "LPT9",
        "report.",
        "report ",
        "invoice\u{202e}fdp.command",
        "\u{2066}file\u{2069}.pdf",
    ] {
        assert_eq!(
            download_name(invalid, &url),
            url.path().rsplit('/').next().unwrap()
        );
    }
    assert_eq!(
        download_name("Annual report.pdf", &url),
        "Annual report.pdf"
    );
    let dir = std::env::temp_dir().join(format!("buzz-download-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let first = save_download(&dir, "report.pdf", b"first").unwrap();
    let second = save_download(&dir, "report.pdf", b"second").unwrap();
    assert_eq!(first.file_name().unwrap(), "report.pdf");
    assert_eq!(second.file_name().unwrap(), "report (1).pdf");
    assert_eq!(std::fs::read(&first).unwrap(), b"first");
    assert_eq!(std::fs::read(&second).unwrap(), b"second");
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut value = [0u8; 128];
        let length = unsafe {
            libc::fgetxattr(
                std::fs::File::open(&first).unwrap().as_raw_fd(),
                c"com.apple.quarantine".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert!(length > 0, "missing quarantine mark");
        assert!(std::str::from_utf8(&value[..length as usize])
            .unwrap()
            .starts_with("0081;"));
    }
    #[cfg(target_os = "windows")]
    {
        let stream = format!("{}:Zone.Identifier", first.display());
        assert_eq!(
            std::fs::read(stream).unwrap(),
            b"[ZoneTransfer]\r\nZoneId=3\r\n"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn media_ranges_are_single_and_bounded() {
    assert_eq!(media_range("bytes=0-"), Some((0, 4194303)));
    assert_eq!(media_range("bytes=10-20"), Some((10, 20)));
    assert_eq!(media_range("bytes=100-999999999"), Some((100, 4194403)));
    assert!(media_range(&format!("bytes={}-", u64::MAX)).is_none());
    for value in [
        "bytes=-500",
        "bytes=5-1",
        "bytes=0-1,4-5",
        "items=0-1",
        "bytes=x-",
    ] {
        assert!(media_range(value).is_none(), "{value}");
    }
}

#[test]
fn media_types_render_only_images_video_and_audio() {
    assert_eq!(
        media_type(Some("image/PNG; x=1")),
        ("image/png".into(), false)
    );
    assert_eq!(media_type(Some("video/mp4")), ("video/mp4".into(), false));
    assert_eq!(media_type(Some("audio/mpeg")), ("audio/mpeg".into(), false));
    for value in [
        Some("image/svg+xml"),
        Some("text/html"),
        Some("image/"),
        None,
    ] {
        assert_eq!(
            media_type(value),
            ("application/octet-stream".into(), true),
            "{value:?}"
        );
    }
}

#[tokio::test]
async fn media_proxy_signs_a_fresh_get_and_forwards_only_the_range() {
    let (base, task) = fixture_server(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: text/html\r\nContent-Range: bytes 0-3/10\r\nContent-Length: 4\r\nConnection: close\r\n\r\n<b>x".into(),
    );
    let url = base.join(&format!("/media/{}", "a".repeat(64))).unwrap();
    let response = fetch_media(&TestSigner, url.clone(), Some("bytes=0-3".into()))
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(response.body(), b"<b>x");
    let header = |name| response.headers().get(name).unwrap().to_str().unwrap();
    assert_eq!(header("content-type"), "application/octet-stream");
    assert_eq!(header("content-disposition"), "attachment");
    assert_eq!(header("x-content-type-options"), "nosniff");
    assert_eq!(header("content-range"), "bytes 0-3/10");
    let (headers, _) = task.join().unwrap();
    assert!(headers.starts_with(&format!("GET {} ", url.path())));
    assert!(headers.lines().any(|line| line == "range: bytes=0-3"));
    assert!(!headers.contains("cookie"));
    let server = &url[url::Position::BeforeHost..url::Position::AfterPort];
    assert_strict(&blossom_event(&headers), "get", server);
}

/// Serves `blob` by `Range`, failing the first `failures` requests with 503,
/// and records every upstream `Range` header.
pub(super) fn ranged_media_server(
    blob: Vec<u8>,
    failures: usize,
) -> (Url, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let ranges = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = ranges.clone();
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let mut socket = socket.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let headers = String::from_utf8_lossy(&bytes).into_owned();
            let range = headers
                .lines()
                .find_map(|line| line.strip_prefix("range: bytes="))
                .unwrap()
                .to_owned();
            let count = {
                let mut seen = seen.lock().unwrap();
                seen.push(format!("bytes={range}"));
                seen.len()
            };
            if count <= failures {
                socket
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
                continue;
            }
            let (first, last) = range.split_once('-').unwrap();
            let first: usize = first.parse().unwrap();
            if first >= blob.len() {
                let head = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", blob.len());
                socket.write_all(head.as_bytes()).unwrap();
                continue;
            }
            let last = last.parse::<usize>().unwrap().min(blob.len() - 1);
            let body = &blob[first..=last];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes {first}-{last}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                blob.len(),
                body.len()
            );
            socket.write_all(head.as_bytes()).unwrap();
            socket.write_all(body).unwrap();
        }
    });
    (base, ranges)
}

pub(super) fn media_blob(length: u64) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

#[tokio::test]
async fn tiny_media_reads_share_one_signed_block_fetch() {
    let blob = media_blob(3000);
    let (base, ranges) = ranged_media_server(blob.clone(), 0);
    let url = base
        .join(&format!("/media/{}.mp4", "c".repeat(64)))
        .unwrap();
    let host = TestSigner;
    // AVFoundation's opening reads, two of them racing from separate players.
    let (header, atom) = tokio::join!(
        media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX),
        media_blocks::read(&host, &url, 32, 39, WHOLE_VIDEO_MAX),
    );
    let (header, atom) = (header.unwrap(), atom.unwrap());
    assert_eq!(header.body(), &blob[0..8]);
    assert_eq!(atom.body(), &blob[32..40]);
    let tail = media_blocks::read(
        &host,
        &url,
        2990,
        2990 + 4 * 1024 * 1024 - 1,
        WHOLE_VIDEO_MAX,
    )
    .await
    .unwrap();
    assert_eq!(tail.status(), 206);
    assert_eq!(tail.body(), &blob[2990..]);
    let header = |name| tail.headers().get(name).unwrap().to_str().unwrap();
    assert_eq!(header("content-range"), "bytes 2990-2999/3000");
    assert_eq!(header("content-type"), "video/mp4");
    assert_eq!(header("accept-ranges"), "bytes");
    assert_eq!(header("x-content-type-options"), "nosniff");
    assert_eq!(
        *ranges.lock().unwrap(),
        [format!("bytes=0-{}", media_blocks::BLOCK - 1)]
    );
}

#[tokio::test]
async fn media_reads_span_blocks_and_stop_at_the_blob_end() {
    let block = media_blocks::BLOCK;
    let blob = media_blob(block + 100);
    let (base, ranges) = ranged_media_server(blob.clone(), 0);
    let url = base
        .join(&format!("/media/{}.mp4", "d".repeat(64)))
        .unwrap();
    let host = TestSigner;
    let across = media_blocks::read(&host, &url, block - 4, block + 3, WHOLE_VIDEO_MAX)
        .await
        .unwrap();
    assert_eq!(across.body(), &blob[block as usize - 4..block as usize + 4]);
    assert_eq!(
        across.headers()["content-range"],
        format!("bytes {}-{}/{}", block - 4, block + 3, block + 100)
    );
    assert_eq!(
        media_blocks::read(&host, &url, block + 100, block + 200, WHOLE_VIDEO_MAX)
            .await
            .unwrap_err(),
        416
    );
    assert_eq!(
        *ranges.lock().unwrap(),
        [
            format!("bytes=0-{}", block - 1),
            format!("bytes={block}-{}", 2 * block - 1),
        ]
    );
}

#[tokio::test]
async fn failed_media_blocks_are_fetched_again() {
    let blob = media_blob(64);
    let (base, ranges) = ranged_media_server(blob.clone(), 1);
    let url = base
        .join(&format!("/media/{}.mp4", "e".repeat(64)))
        .unwrap();
    let host = TestSigner;
    assert_eq!(
        media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
            .await
            .unwrap_err(),
        503
    );
    let retried = media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
        .await
        .unwrap();
    assert_eq!(retried.body(), &blob[..8]);
    assert_eq!(ranges.lock().unwrap().len(), 2);
}

/// Answers each connection with the next canned response, ignoring the request.
fn scripted_media_server(
    responses: Vec<Vec<u8>>,
) -> (Url, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for (socket, response) in listener.incoming().zip(responses) {
            let mut socket = socket.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The client may hang up on an oversized body.
            let _ = socket.write_all(&response);
        }
    });
    (base, served)
}

pub(super) fn media_response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\n{headers}Connection: close\r\n\r\n"
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[tokio::test]
async fn range_ignoring_upstreams_keep_the_whole_response_fallback() {
    let block = media_blocks::BLOCK;
    for (name, length) in [("f", 64), ("0", block + 64)] {
        let blob = media_blob(length);
        let whole = String::from_utf8_lossy(&media_response(
            "200 OK",
            &format!("Content-Length: {length}\r\n"),
            &[],
        ))
        .replace("video/mp4", "audio/mpeg")
        .into_bytes();
        let whole = [whole, blob.clone()].concat();
        let (base, served) = scripted_media_server(vec![whole.clone(), whole]);
        let url = base
            .join(&format!("/media/{}.mp3", name.repeat(64)))
            .unwrap();
        let host = TestSigner;
        for _ in 0..2 {
            let response = media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.body(), &blob);
            assert!(response.headers().get("content-range").is_none());
        }
        // A whole response is never cached as a block.
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}

/// Spool tests share the process-wide spool list and open-file count.
pub(super) static SPOOL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Ignores `Range` and sends `blob` whole as a 200, with `length` as its
/// `Content-Length`, in 64 KiB writes `pace` apart. Counts requests and
/// connections the client hung up on.
pub(super) fn whole_media_server(
    blob: Vec<u8>,
    length: Option<u64>,
    pace: Duration,
) -> (
    Url,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let (served, hung_up) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (count, hangups) = (served.clone(), hung_up.clone());
    let blob = Arc::new(blob);
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let mut socket = socket.unwrap();
            let (blob, count, hangups) = (blob.clone(), count.clone(), hangups.clone());
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                assert!(String::from_utf8_lossy(&bytes).contains("range: bytes="));
                count.fetch_add(1, Ordering::SeqCst);
                let length = length.map_or(String::new(), |length| {
                    format!("Content-Length: {length}\r\n")
                });
                let head = media_response("200 OK", &length, &[]);
                let sent = socket.write_all(&head).and_then(|()| {
                    for chunk in blob.chunks(64 * 1024) {
                        socket.write_all(chunk)?;
                        socket.flush()?;
                        std::thread::sleep(pace);
                    }
                    Ok(())
                });
                if sent.is_err() {
                    hangups.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    (base, served, hung_up)
}

/// Waits for every spool file to be deleted, as idle eviction must do.
pub(super) async fn spool_files_deleted() {
    for _ in 0..100 {
        if media_spool::open_files() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} spool files remain", media_spool::open_files());
}

#[tokio::test]
async fn range_ignoring_videos_are_spooled_and_answered_as_exact_ranges() {
    let _serial = SPOOL_TESTS.lock().await;
    let block = media_blocks::BLOCK;
    let length = 3 * block + 17;
    let blob = media_blob(length);
    let (base, served, _) = whole_media_server(blob.clone(), Some(length), Duration::ZERO);
    let url = base
        .join(&format!("/media/{}.mp4", "2".repeat(64)))
        .unwrap();
    let host = TestSigner;
    let limit = WHOLE_VIDEO_MAX;
    // Two players open the video at once; they share one spool.
    let (header, tail) = tokio::join!(
        media_blocks::read(&host, &url, 0, 7, limit),
        media_blocks::read(&host, &url, length - 8, length + 100, limit),
    );
    let (header, tail) = (header.unwrap(), tail.unwrap());
    assert_eq!(header.status(), 206);
    assert_eq!(header.body(), &blob[..8]);
    assert_eq!(
        header.headers()["content-range"],
        format!("bytes 0-7/{length}")
    );
    assert_eq!(header.headers()["content-type"], "video/mp4");
    assert_eq!(header.headers()["accept-ranges"], "bytes");
    assert_eq!(header.headers()["x-content-type-options"], "nosniff");
    assert_eq!(tail.body(), &blob[length as usize - 8..]);
    assert_eq!(
        tail.headers()["content-range"],
        format!("bytes {}-{}/{length}", length - 8, length - 1)
    );
    // A seek across blocks reads the same file.
    let seek = media_blocks::read(&host, &url, 2 * block - 4, 2 * block + 3, limit)
        .await
        .unwrap();
    assert_eq!(
        seek.body(),
        &blob[2 * block as usize - 4..2 * block as usize + 4]
    );
    assert_eq!(
        media_blocks::read(&host, &url, length, length + 8, limit)
            .await
            .unwrap_err(),
        416
    );
    assert!(served.load(std::sync::atomic::Ordering::SeqCst) <= 2);
    spool_files_deleted().await;
}

#[tokio::test]
async fn whole_videos_follow_the_configured_limit_boundary() {
    let _serial = SPOOL_TESTS.lock().await;
    let host = TestSigner;
    // A configured limit other than the default.
    let limit = 2 * media_blocks::BLOCK + 5;
    for (name, length, sent_length, expected) in [
        ("3", limit, true, Ok(())),
        ("4", limit + 1, true, Err(413)),
        ("5", limit, false, Ok(())),
        ("6", limit + 1, false, Err(413)),
    ] {
        let blob = media_blob(length);
        let (base, served, _) =
            whole_media_server(blob.clone(), sent_length.then_some(length), Duration::ZERO);
        let url = base
            .join(&format!("/media/{}.mp4", name.repeat(64)))
            .unwrap();
        let read = media_blocks::read(&host, &url, length - 4, length - 1, limit).await;
        match expected {
            Ok(()) => {
                let read = read.unwrap();
                assert_eq!(read.body(), &blob[length as usize - 4..], "{name}");
                assert_eq!(
                    read.headers()["content-range"],
                    format!("bytes {}-{}/{length}", length - 4, length - 1)
                );
            }
            Err(status) => {
                assert_eq!(read.unwrap_err(), status, "{name}");
                // A rejected video is fetched again, never answered from a spool.
                assert_eq!(
                    media_blocks::read(&host, &url, 0, 7, limit)
                        .await
                        .unwrap_err(),
                    status
                );
                assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 2);
            }
        }
    }
    spool_files_deleted().await;
}

#[tokio::test]
async fn truncated_whole_videos_fail_and_are_fetched_again() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(media_blocks::BLOCK + 9);
    let length = blob.len() as u64 + 10;
    let (base, served, _) = whole_media_server(blob, Some(length), Duration::ZERO);
    let url = base
        .join(&format!("/media/{}.mp4", "7".repeat(64)))
        .unwrap();
    let host = TestSigner;
    for _ in 0..2 {
        assert_eq!(
            media_blocks::read(&host, &url, length - 4, length - 1, WHOLE_VIDEO_MAX)
                .await
                .unwrap_err(),
            502
        );
    }
    assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 2);
    spool_files_deleted().await;
}

#[tokio::test]
async fn abandoned_whole_video_downloads_stop_and_delete_their_file() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(16 * media_blocks::BLOCK);
    let length = blob.len() as u64;
    // 64 KiB per 20 ms: the whole video would take over five seconds.
    let (base, _, hung_up) =
        whole_media_server(blob.clone(), Some(length), Duration::from_millis(20));
    let url = base
        .join(&format!("/media/{}.mp4", "8".repeat(64)))
        .unwrap();
    let host = TestSigner;
    let first = media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
        .await
        .unwrap();
    assert_eq!(first.body(), &blob[..8]);
    assert_eq!(media_spool::open_files(), 1);
    // The player closes: no further reads arrive.
    spool_files_deleted().await;
    for _ in 0..100 {
        if hung_up.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the abandoned download kept its upstream connection");
}

/// Waits until the server has seen `count` hang-ups.
pub(super) async fn hung_up(hung_up: &std::sync::atomic::AtomicUsize, count: usize) {
    for _ in 0..100 {
        if hung_up.load(std::sync::atomic::Ordering::SeqCst) >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("an abandoned download kept its upstream connection");
}

pub(super) async fn spool_files_reach(count: usize) {
    for _ in 0..100 {
        if media_spool::open_files() == count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{} spool files, expected {count}",
        media_spool::open_files()
    );
}

#[tokio::test]
async fn a_third_live_video_evicts_the_oldest_download_and_its_file() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(media_blocks::BLOCK);
    let length = blob.len() as u64;
    // Without a length every read waits for the whole copy, about 0.6 s here.
    let (base, _, hangups) = whole_media_server(blob.clone(), None, Duration::from_millis(40));
    let host = std::sync::Arc::new(TestSigner);
    let load = |name: &str| {
        let url = base
            .join(&format!("/media/{}.mp4", name.repeat(64)))
            .unwrap();
        let host = host.clone();
        tokio::spawn(async move {
            media_blocks::read(&host, &url, length - 4, length - 1, WHOLE_VIDEO_MAX).await
        })
    };
    let first = load("a");
    spool_files_reach(1).await;
    let second = load("b");
    spool_files_reach(2).await;
    let third = load("c");
    // Admitting the third waits for the oldest file to be deleted.
    let mut most = 0;
    while !third.is_finished() {
        most = most.max(media_spool::open_files());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(most <= 2, "{most} spool files");
    assert_eq!(first.await.unwrap().unwrap_err(), 503);
    for load in [second.await.unwrap(), third.await.unwrap()] {
        assert_eq!(load.unwrap().body(), &blob[length as usize - 4..]);
    }
    hung_up(&hangups, 1).await;
    spool_files_deleted().await;
}

/// Sends `blob` whole to the first two requests, but only once both have
/// arrived, so their two 200 heads reach the client together.
fn paired_whole_media_server(blob: Vec<u8>) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    std::thread::spawn(move || {
        let mut sockets: Vec<_> = listener
            .incoming()
            .take(2)
            .map(|socket| socket.unwrap())
            .collect();
        for socket in &mut sockets {
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
        }
        let length = format!("Content-Length: {}\r\n", blob.len());
        for socket in &mut sockets {
            socket
                .write_all(&media_response("200 OK", &length, &[]))
                .unwrap();
        }
        let blob = std::sync::Arc::new(blob);
        for mut socket in sockets {
            let blob = blob.clone();
            std::thread::spawn(move || socket.write_all(&blob));
        }
    });
    base
}

#[tokio::test]
async fn concurrent_opens_of_one_new_video_evict_only_one_player() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(2 * media_blocks::BLOCK + 17);
    let length = blob.len() as u64;
    // Without a length every read of these waits for the whole copy (1.3 s).
    let (old, _, _) = whole_media_server(blob.clone(), None, Duration::from_millis(40));
    let new = paired_whole_media_server(blob.clone());
    let url = |base: &Url, name: &str| {
        base.join(&format!("/media/{}.mp4", name.repeat(64)))
            .unwrap()
    };
    let urls = [url(&old, "d"), url(&old, "e"), url(&new, "f")];
    let host = std::sync::Arc::new(TestSigner);
    let load = |url: &Url, start: u64, end: u64| {
        let (host, url) = (host.clone(), url.clone());
        tokio::spawn(
            async move { media_blocks::read(&host, &url, start, end, WHOLE_VIDEO_MAX).await },
        )
    };
    let first = load(&urls[0], length - 4, length - 1);
    spool_files_reach(1).await;
    let second = load(&urls[1], length - 4, length - 1);
    spool_files_reach(2).await;
    // Holding both spools keeps their files, so the first admission of the
    // third video waits for a file while the second one arrives.
    let pinned = [
        media_spool::cached(&urls[0]).unwrap(),
        media_spool::cached(&urls[1]).unwrap(),
    ];
    // As the players' connections would, past their reads.
    let holds = [
        media_spool::hold(&urls[0]).unwrap(),
        media_spool::hold(&urls[1]).unwrap(),
    ];
    // Reads in two blocks of a third video fetch separately; both upstream
    // 200s reach admission together while both files are taken.
    let opening = load(&urls[2], 0, 7);
    let seek = load(&urls[2], length - 4, length - 1);
    while !first.is_finished() && !second.is_finished() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Room for a second, unneeded eviction to show before the files free up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !(first.is_finished() && second.is_finished()),
        "one player's spool is enough to make room"
    );
    drop(pinned);
    assert_eq!(opening.await.unwrap().unwrap().body(), &blob[..8]);
    assert_eq!(
        seek.await.unwrap().unwrap().body(),
        &blob[length as usize - 4..]
    );
    // The new player's connection, while the old players' copies finish.
    let new_hold = media_spool::hold(&urls[2]).unwrap();
    let old_reads = [first.await.unwrap(), second.await.unwrap()];
    let evicted = old_reads
        .iter()
        .filter(|read| read.as_ref().err() == Some(&503))
        .count();
    assert_eq!(evicted, 1);
    for read in old_reads.into_iter().flatten() {
        assert_eq!(read.body(), &blob[length as usize - 4..]);
    }
    assert_eq!(media_spool::open_files(), 2);
    drop(new_hold);
    drop(holds);
    spool_files_deleted().await;
}

#[tokio::test]
async fn a_closed_player_waiting_for_an_unknown_length_stops_the_download() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(16 * media_blocks::BLOCK);
    // Over five seconds to copy.
    let (base, _, hangups) = whole_media_server(blob, None, Duration::from_millis(20));
    let url = base
        .join(&format!("/media/{}.mp4", "9".repeat(64)))
        .unwrap();
    let host = std::sync::Arc::new(TestSigner);
    let read = {
        let (host, url) = (host.clone(), url.clone());
        tokio::spawn(async move { media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX).await })
    };
    spool_files_reach(1).await;
    // The player's connection closes, dropping the read it waits on.
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    spool_files_deleted().await;
    hung_up(&hangups, 1).await;
}

#[tokio::test]
async fn a_closed_player_waiting_on_a_seek_stops_the_download() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(16 * media_blocks::BLOCK);
    let length = blob.len() as u64;
    let (base, _, hangups) =
        whole_media_server(blob.clone(), Some(length), Duration::from_millis(20));
    let url = base
        .join(&format!("/media/{}.mp4", "a".repeat(64)))
        .unwrap();
    let host = TestSigner;
    let first = media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
        .await
        .unwrap();
    assert_eq!(first.body(), &blob[..8]);
    // A seek near the end waits for bytes; then the player closes.
    let seek = media_blocks::read(&host, &url, length - 8, length - 1, WHOLE_VIDEO_MAX);
    assert!(tokio::time::timeout(Duration::from_millis(500), seek)
        .await
        .is_err());
    spool_files_deleted().await;
    hung_up(&hangups, 1).await;
}

#[tokio::test]
async fn a_waiting_read_keeps_a_slow_download_alive() {
    let _serial = SPOOL_TESTS.lock().await;
    let blob = media_blob(8 * media_blocks::BLOCK);
    let length = blob.len() as u64;
    // About 2.6 s to copy, over twice the idle window.
    let (base, _, hangups) = whole_media_server(blob.clone(), None, Duration::from_millis(20));
    let url = base
        .join(&format!("/media/{}.mp4", "b".repeat(64)))
        .unwrap();
    let host = TestSigner;
    let read = media_blocks::read(&host, &url, length - 4, length - 1, WHOLE_VIDEO_MAX).await;
    assert_eq!(read.unwrap().body(), &blob[length as usize - 4..]);
    assert_eq!(hangups.load(std::sync::atomic::Ordering::SeqCst), 0);
    spool_files_deleted().await;
}

#[tokio::test]
async fn invalid_partial_media_blocks_are_rejected_and_retried() {
    let block = media_blocks::BLOCK;
    let blob = media_blob(3000);
    let oversized = media_blob(2 * block);
    let partial = |range: &str, body: &[u8]| {
        let length = format!("Content-Length: {}\r\n", body.len());
        let range = if range.is_empty() {
            String::new()
        } else {
            format!("Content-Range: {range}\r\n")
        };
        media_response("206 Partial Content", &format!("{range}{length}"), body)
    };
    let cases = [
        (partial("", &blob), 502),
        (partial("bytes 0-x/3000", &blob), 502),
        (partial("bytes 8-15/3000", &blob[8..16]), 502),
        (partial("bytes 0-7/3000", &blob[..8]), 502),
        (
            partial(
                &format!("bytes 0-{}/{}", 2 * block - 1, 4 * block),
                &oversized,
            ),
            413,
        ),
        // No Content-Length: the 1 MiB cap applies while buffering.
        (
            media_response(
                "206 Partial Content",
                &format!("Content-Range: bytes 0-{}/{}\r\n", 2 * block - 1, 4 * block),
                &oversized,
            ),
            413,
        ),
    ];
    let expected: Vec<u16> = cases.iter().map(|(_, status)| *status).collect();
    let mut responses: Vec<Vec<u8>> = cases.into_iter().map(|(response, _)| response).collect();
    responses.push(partial("bytes 0-2999/3000", &blob));
    let (base, served) = scripted_media_server(responses);
    let url = base
        .join(&format!("/media/{}.mp4", "1".repeat(64)))
        .unwrap();
    let host = TestSigner;
    for status in expected.iter() {
        assert_eq!(
            media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
                .await
                .unwrap_err(),
            *status
        );
    }
    let read = media_blocks::read(&host, &url, 0, 7, WHOLE_VIDEO_MAX)
        .await
        .unwrap();
    assert_eq!(read.status(), 206);
    assert_eq!(read.body(), &blob[..8]);
    assert_eq!(read.headers()["content-range"], "bytes 0-7/3000");
    // Rejections were never cached; the valid block now is.
    assert_eq!(
        media_blocks::read(&host, &url, 2992, 2999, WHOLE_VIDEO_MAX)
            .await
            .unwrap()
            .body(),
        &blob[2992..]
    );
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        expected.len() + 1
    );
}

#[tokio::test]
async fn media_proxy_passes_relay_denials_through_without_a_body() {
    let (base, task) = fixture_server(
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into(),
    );
    let url = base.join(&format!("/media/{}", "b".repeat(64))).unwrap();
    assert_eq!(fetch_media(&TestSigner, url, None).await.unwrap_err(), 401);
    task.join().unwrap();
}

#[tokio::test]
async fn snapshot_media_read_preserves_auth_and_enforces_the_tighter_file_cap() {
    for (body, limit, expected) in [("safe", 4, Ok(())), ("large", 4, Err(413u16))] {
        let (base, task) = fixture_server(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
        ));
        let url = base.join(&format!("/media/{}", "a".repeat(64))).unwrap();
        let response = fetch_media_bounded(&TestSigner, url.clone(), None, limit).await;
        assert_eq!(
            response.as_ref().map(|_| ()).map_err(|status| *status),
            expected
        );
        if let Ok(response) = response {
            assert_eq!(response.body(), body.as_bytes());
        }
        let (headers, _) = task.join().unwrap();
        let server = &url[url::Position::BeforeHost..url::Position::AfterPort];
        assert_strict(&blossom_event(&headers), "get", server);
    }
}
