use super::*;

fn options(path: &str) -> OAuthRequest {
    OAuthRequest {
        authorization_url: "https://provider.example/authorize?client_id=buzz&scope=a%20b&scope=c"
            .into(),
        callback_path: path.into(),
        use_state: None,
    }
}

#[derive(Debug, PartialEq)]
struct Started {
    id: String,
    callback_url: String,
    authorization: url::Url,
}

fn start(host: &OAuthCallbackHost, request: OAuthRequest) -> Result<Started, String> {
    let mut opened = None;
    let attempt = host.begin(request, |url| {
        opened = Some(url::Url::parse(url).unwrap());
        Ok(())
    })?;
    let authorization = opened.unwrap();
    assert_eq!(
        serde_json::to_value(&attempt).unwrap(),
        serde_json::json!({"id": attempt.id, "callbackUrl": attempt.callback_url})
    );
    let callback_url = attempt.callback_url;
    assert_eq!(
        authorization
            .query_pairs()
            .find(|(name, _)| name == "redirect_uri")
            .unwrap()
            .1,
        callback_url,
    );
    Ok(Started {
        id: attempt.id,
        callback_url,
        authorization,
    })
}

fn begin(host: &OAuthCallbackHost, path: String) -> Result<Started, String> {
    start(
        host,
        OAuthRequest {
            use_state: Some(false),
            ..options(&path)
        },
    )
}

// Existing transport-only cases exercise the explicit custom-protocol mode.
fn parse_callback(
    bytes: &[u8],
    authority: &str,
    path: &str,
) -> (u16, &'static str, Option<OAuthCallback>) {
    super::parse_callback(bytes, authority, path, None)
}

fn response(parameters: &[(&str, &str)]) -> OAuthCallback {
    OAuthCallback {
        parameters: parameters
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into()))
            .collect(),
    }
}

#[test]
fn selected_path_must_be_a_canonical_absolute_path_without_query_or_fragment() {
    let state = OAuthCallbackHost::default();
    for path in [
        "",
        "callback",
        "//evil.example/callback",
        "/callback?x=y",
        "/callback#x",
        "/a/../callback",
        "/\\evil.example/callback",
        "/callback with spaces",
    ] {
        assert_eq!(
            begin(&state, path.into()),
            Err("Invalid callback path".into())
        );
    }
    assert!(state.0.lock().unwrap().is_none());
}

fn request(authority: &str, target: &str, extra: &str) -> Vec<u8> {
    format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n").into_bytes()
}

#[test]
fn callback_requires_the_exact_loopback_host_and_selected_path() {
    let host = "127.0.0.1:4567";
    let path = "/callback/random";
    for bytes in [
        request("evil.example", "/callback/random?code=x", ""),
        request(host, "//evil.example/callback/random?code=x", ""),
        request(
            host,
            "/callback/random?code=x",
            "Origin: https://evil.example\r\n",
        ),
        request(host, "/callback/random?code=x", "Host: 127.0.0.1:4567\r\n"),
        request(host, "/callback/random?code=x#fragment", ""),
        b"POST /callback/random?code=x HTTP/1.1\r\nHost: 127.0.0.1:4567\r\n\r\n".to_vec(),
    ] {
        let (status, _, code) = parse_callback(&bytes, host, path);
        assert_eq!(status, 400);
        assert!(code.is_none());
    }
    for target in ["/callback/wrong?code=x", "/wrong/../callback/random?code=x"] {
        assert_eq!(
            parse_callback(&request(host, target, ""), host, path).0,
            404
        );
    }
}

#[test]
fn callback_rejects_missing_empty_duplicate_or_oversized_code() {
    let host = "127.0.0.1:4567";
    let path = "/callback/random";
    for query in [
        "",
        "?code=",
        "?code=a&code=b",
        &format!("?code={}", "a".repeat(4097)),
    ] {
        let (_, _, code) =
            parse_callback(&request(host, &format!("{path}{query}"), ""), host, path);
        assert!(code.is_none());
    }
}

async fn send(url: &url::Url, target: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    stream
        .write_all(&request(
            &format!("127.0.0.1:{}", url.port().unwrap()),
            target,
            extra,
        ))
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn real_listener_preserves_callback_parameters_and_closes_after_completion() {
    let state = OAuthCallbackHost::default();
    let attempt = begin(&state, "/oauth2redirect/provider".into()).unwrap();
    let id = attempt.id.as_str();
    let url = url::Url::parse(&attempt.callback_url).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert!(!url.as_str().contains(id));
    let wait = state.wait(id);
    let browser = async {
        assert!(send(&url, "/callback/wrong?code=x", "")
            .await
            .starts_with("HTTP/1.1 404"));
        assert!(send(&url, &format!("{}?code=a&code=b", url.path()), "")
            .await
            .starts_with("HTTP/1.1 400"));
        // Keep a second connection queued behind the callback. Its identity stays
        // tied to this listener even if another test later reuses the port.
        let address = ("127.0.0.1", url.port().unwrap());
        let mut callback = TcpStream::connect(address).await.unwrap();
        let queued = TcpStream::connect(address).await.unwrap();
        callback
            .write_all(&request(
                &format!("127.0.0.1:{}", url.port().unwrap()),
                &format!(
                    "{}?code=one%2Btime&state=attempt%2Bstate&extra=a&extra=b",
                    url.path()
                ),
                "",
            ))
            .await
            .unwrap();
        let mut response = String::new();
        callback.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Cache-Control: no-store"));
        assert!(response.contains("Referrer-Policy: no-referrer"));
        assert!(response.contains("<title>Sign-in callback</title>"));
        assert!(response.contains("Return to the app"));
        assert!(!response.contains("one+time"));
        assert!(!response.contains("attempt"));
        assert!(!response.contains("one%2Btime"));
        queued
    };
    let (result, mut queued) = tokio::join!(wait, browser);
    let expected = response(&[
        ("code", "one+time"),
        ("state", "attempt+state"),
        ("extra", "a"),
        ("extra", "b"),
    ]);
    assert_eq!(
        serde_json::to_value(&expected).unwrap(),
        serde_json::json!({"parameters": [["code", "one+time"], ["state", "attempt+state"], ["extra", "a"], ["extra", "b"]]})
    );
    assert_eq!(result, Ok(expected));
    assert!(state.0.lock().unwrap().is_none());
    let mut byte = [0];
    let closed = tokio::time::timeout(Duration::from_secs(5), queued.read(&mut byte))
        .await
        .expect("completed callback must close queued connections");
    match closed {
        Ok(0) => (),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
        other => panic!("expected queued connection to close, got {other:?}"),
    }
    assert!(state.wait(id).await.is_err());
}

#[tokio::test]
async fn cancellation_unblocks_wait_and_stale_cancel_does_not_end_a_new_attempt() {
    let state = OAuthCallbackHost::default();
    let first = start(&state, options("/callback/legacy-nonce")).unwrap().id;
    assert!(begin(&state, "/callback/other".into()).is_err());
    let wait = state.wait(&first);
    let cancel = async {
        state.cancel(&first).unwrap();
    };
    let (result, ()) = tokio::join!(wait, cancel);
    assert_eq!(result, Err("Sign-in canceled.".into()));
    let attempt = begin(&state, "/callback/new-nonce".into()).unwrap();
    let second = attempt.id;
    assert_ne!(first, second);
    let url = url::Url::parse(&attempt.callback_url).unwrap();
    state.cancel(&first).unwrap();
    let target = format!("{}?code=next", url.path());
    let (result, _) = tokio::join!(state.wait(&second), send(&url, &target, ""));
    assert_eq!(result, Ok(response(&[("code", "next")])));
}

#[tokio::test(start_paused = true)]
async fn abandoned_listener_expires_and_can_be_replaced() {
    let state = OAuthCallbackHost::default();
    let id = start(&state, options("/oauth2redirect/provider"))
        .unwrap()
        .id;
    assert_eq!(
        state.wait(&id).await,
        Err("Sign-in timed out. Try again.".into())
    );
    let next = start(&state, options("/oauth2redirect/provider"))
        .unwrap()
        .id;
    state.cancel(&next).unwrap();
}

#[tokio::test]
async fn abandon_cancels_wait_and_allows_new_sign_in() {
    let state = OAuthCallbackHost::default();
    let first = start(&state, options("/callback/old")).unwrap().id;
    let reload = async {
        state.abandon().unwrap();
        let attempt = begin(&state, "/callback/new".into()).unwrap();
        let next = attempt.id;
        let url = url::Url::parse(&attempt.callback_url).unwrap();
        let target = format!("{}?code=new-login", url.path());
        let (result, _) = tokio::join!(state.wait(&next), send(&url, &target, ""));
        assert_eq!(result, Ok(response(&[("code", "new-login")])));
    };
    let (result, ()) = tokio::join!(biased; state.wait(&first), reload);
    assert_eq!(result, Err("Sign-in canceled.".into()));
}

#[test]
fn startup_rejects_unsafe_urls_and_conflicting_owned_parameters_before_launch() {
    let host = OAuthCallbackHost::default();
    let invalid = [
        OAuthRequest {
            authorization_url: "http://provider.example/authorize".into(),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "https://user:secret@provider.example/authorize".into(),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "https://provider.example/authorize#fragment".into(),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "not a URL".into(),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "https://provider.example/authorize?st%61te=caller".into(),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "https://provider.example/authorize?state=caller".into(),
            use_state: Some(false),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "https://provider.example/authorize?redirect_uri=caller".into(),
            ..options("/callback")
        },
        OAuthRequest {
            authorization_url: "https://provider.example/authorize?redirect%5Furi=caller".into(),
            ..options("/callback")
        },
    ];
    for request in invalid {
        assert!(host
            .begin(request, |_| panic!("Invalid startup must not launch"))
            .is_err());
        assert!(host.0.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn default_state_rejects_invalid_success_and_error_callbacks_then_completes_once() {
    let host = OAuthCallbackHost::default();
    let attempt = start(&host, options("/oauth2redirect/provider")).unwrap();
    let authorization = &attempt.authorization;
    assert_eq!(
        authorization
            .query_pairs()
            .filter(|(name, _)| name == "scope")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>(),
        ["a b", "c"]
    );
    assert_eq!(
        authorization
            .query_pairs()
            .filter(|(name, _)| name == "redirect_uri")
            .count(),
        1
    );
    let state = authorization
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(URL_SAFE_NO_PAD.decode(&state).unwrap().len(), 32);
    assert_ne!(state, attempt.id);
    let url = url::Url::parse(&attempt.callback_url).unwrap();
    assert!(!url.as_str().contains(&state));
    assert!(!url.as_str().contains(&attempt.id));
    for response in ["code=one", "error=access_denied"] {
        for suffix in [
            String::new(),
            "&state=wrong".into(),
            format!("&state={state}&state={state}"),
        ] {
            let html = send(&url, &format!("{}?{response}{suffix}", url.path()), "").await;
            assert!(html.starts_with("HTTP/1.1 400"));
            assert!(html.contains("Invalid callback"));
            assert!(!html.contains(&state));
            assert!(!html.contains("access_denied"));
            assert!(host.0.lock().unwrap().is_some());
        }
    }
    let target = format!("{}?code=one&state={state}", url.path());
    let (result, html) = tokio::join!(host.wait(&attempt.id), send(&url, &target, ""));
    assert_eq!(result, Ok(response(&[("code", "one"), ("state", &state)])));
    assert!(html.starts_with("HTTP/1.1 200"));
    assert!(!html.contains(&state));
    assert!(host.0.lock().unwrap().is_none());
    assert!(host.wait(&attempt.id).await.is_err());
    let next = start(&host, options("/oauth2redirect/provider")).unwrap();
    let next_state = next
        .authorization
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    assert_ne!(state, next_state);
    let next_url = url::Url::parse(&next.callback_url).unwrap();
    assert!(send(&next_url, &target, "")
        .await
        .starts_with("HTTP/1.1 400"));
    let next_target = format!(
        "{}?error=access_denied&state={next_state}&error_description=private+detail&error_uri=https%3A%2F%2Fprovider.example%2Fhelp",
        next_url.path()
    );
    let (result, html) = tokio::join!(host.wait(&next.id), send(&next_url, &next_target, ""));
    assert_eq!(
        result,
        Ok(response(&[
            ("error", "access_denied"),
            ("state", &next_state),
            ("error_description", "private detail"),
            ("error_uri", "https://provider.example/help")
        ]))
    );
    assert!(html.starts_with("HTTP/1.1 400"));
    assert!(!html.contains("access_denied"));
    assert!(!html.contains(&next_state));
    assert!(!html.contains("private detail"));
    assert!(!html.contains("provider.example/help"));
    assert!(host.0.lock().unwrap().is_none());
}

#[tokio::test]
async fn redirect_uri_is_used_even_with_state_disabled() {
    let host = OAuthCallbackHost::default();
    let attempt = start(
        &host,
        OAuthRequest {
            use_state: Some(false),
            ..options("/callback/random")
        },
    )
    .unwrap();
    assert!(!attempt
        .authorization
        .query_pairs()
        .any(|(name, _)| name == "state"));
    assert_eq!(
        attempt
            .authorization
            .query_pairs()
            .filter(|(name, _)| name == "redirect_uri")
            .count(),
        1
    );
    let url = url::Url::parse(&attempt.callback_url).unwrap();
    // Disabled means no validation, even for duplicate unsolicited state.
    let target = format!("{}?code=one&state=a&state=b", url.path());
    let (result, _) = tokio::join!(host.wait(&attempt.id), send(&url, &target, ""));
    assert_eq!(
        result,
        Ok(response(&[("code", "one"), ("state", "a"), ("state", "b")]))
    );
}

#[tokio::test]
async fn launch_failure_cleans_up_but_cannot_cancel_a_replacement_after_reload() {
    let host = OAuthCallbackHost::default();
    assert_eq!(
        host.begin(options("/callback"), |_| Err("private-launch-error".into())),
        Err("Could not open the sign-in browser".into())
    );
    assert!(host.0.lock().unwrap().is_none());
    let mut next = None;
    assert_eq!(
        host.begin(options("/callback"), |_| {
            host.abandon()?;
            next = Some(start(&host, options("/callback"))?);
            Err("private-launch-error".into())
        }),
        Err("Could not open the sign-in browser".into())
    );
    let next = next.unwrap();
    assert_eq!(host.0.lock().unwrap().as_ref().unwrap().id, next.id);
    host.cancel(&next.id).unwrap();
}
