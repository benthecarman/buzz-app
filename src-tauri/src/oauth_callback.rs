//! Tauri commands for browser sign-in. The loopback listener and its
//! contract live in [`buzz_desktop::oauth_callback`].
pub(crate) use buzz_desktop::oauth_callback::OAuthCallbackHost;
use buzz_desktop::oauth_callback::{OAuthAttempt, OAuthCallback, OAuthRequest};
use tauri_plugin_opener::OpenerExt;

/// Ends the attempt when the main webview starts a new document.
pub(crate) fn document_load(
    host: &OAuthCallbackHost,
    label: &str,
    event: tauri::webview::PageLoadEvent,
) -> Result<(), String> {
    if label == "main" && event == tauri::webview::PageLoadEvent::Started {
        host.abandon()?;
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn oauth_callback_begin<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, OAuthCallbackHost>,
    authorization_url: String,
    callback_path: String,
    use_state: Option<bool>,
) -> Result<OAuthAttempt, String> {
    // Register and launch inline so reload cleanup cannot overtake a queued begin.
    // Enter the runtime for the Tokio listener; startup does not await anything.
    tauri::async_runtime::block_on(async {
        state.begin(
            OAuthRequest {
                authorization_url,
                callback_path,
                use_state,
            },
            |url| {
                app.opener()
                    .open_url(url, None::<&str>)
                    .map_err(|_| "Could not open the sign-in browser".into())
            },
        )
    })
}
#[tauri::command]
pub(crate) async fn oauth_callback_wait(
    state: tauri::State<'_, OAuthCallbackHost>,
    id: String,
) -> Result<OAuthCallback, String> {
    state.wait(&id).await
}
#[tauri::command]
pub(crate) fn oauth_callback_cancel(
    state: tauri::State<'_, OAuthCallbackHost>,
    id: String,
) -> Result<(), String> {
    state.cancel(&id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::webview::PageLoadEvent::{Finished, Started};

    fn begin(host: &OAuthCallbackHost) -> Result<OAuthAttempt, String> {
        host.begin(
            OAuthRequest {
                authorization_url: "https://provider.example/authorize".into(),
                callback_path: "/callback".into(),
                use_state: None,
            },
            |_| Ok(()),
        )
    }

    #[tokio::test]
    async fn only_a_main_document_load_ends_the_attempt() {
        // Async Tauri commands queue startup, allowing reload cleanup to overtake it.
        let _: fn(_, _, _, _, _) -> Result<OAuthAttempt, String> =
            oauth_callback_begin::<tauri::test::MockRuntime>;
        let host = OAuthCallbackHost::default();
        let first = begin(&host).unwrap().id;
        for (label, event) in [("browser-content", Started), ("main", Finished)] {
            document_load(&host, label, event).unwrap();
            assert_eq!(
                begin(&host),
                Err("An OAuth callback is already pending".into())
            );
        }
        document_load(&host, "main", Started).unwrap();
        let next = begin(&host).unwrap().id;
        assert_ne!(first, next);
        assert_eq!(
            host.wait(&first).await,
            Err("Sign-in is no longer available".into())
        );
        host.cancel(&next).unwrap();
    }
}
