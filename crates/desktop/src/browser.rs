//! Rules for Buzz Browser: which URLs it may open and where its view may sit.
use url::Url;

const MAXIMUM_URL_BYTES: usize = 2048;

/// Decides which URLs Buzz Browser may load: credential-free HTTP(S)
/// websites, never the app's own origins.
#[derive(Clone)]
pub struct NavigationPolicy {
    development_url: Option<Url>,
}

impl NavigationPolicy {
    /// Creates a policy. `development_url` is the dev server that serves the
    /// app in debug builds, which must not open as a website.
    pub fn new(development_url: Option<Url>) -> Self {
        Self { development_url }
    }

    /// Parses `target` and returns it normalized, or a message to show the
    /// user.
    pub fn validate(&self, target: &str) -> Result<Url, String> {
        if target.len() > MAXIMUM_URL_BYTES {
            return Err("Browser URLs cannot exceed 2048 bytes".into());
        }
        let url = Url::parse(target)
            .map_err(|_| "Enter a complete http:// or https:// URL".to_string())?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("Buzz Browser supports HTTP and HTTPS websites only".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("URLs containing credentials are not supported".into());
        }
        if matches!(
            url.host_str(),
            Some("tauri.localhost" | "asset.localhost" | "ipc.localhost")
        ) || self
            .development_url
            .as_ref()
            .is_some_and(|development| development.origin() == url.origin())
        {
            return Err("Buzz application URLs cannot be opened as websites".into());
        }
        if url.as_str().len() > MAXIMUM_URL_BYTES {
            return Err("Browser URLs cannot exceed 2048 bytes after URL encoding".into());
        }
        Ok(url)
    }
}

/// Coordinates relative to the main webview's CSS viewport.
#[derive(Clone, Copy, Debug, serde::Deserialize, PartialEq)]
pub struct BrowserBounds {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width, which must be positive.
    pub width: f64,
    /// Height, which must be positive.
    pub height: f64,
}

impl BrowserBounds {
    /// Rejects non-finite, negative or empty bounds.
    pub fn validate(self) -> Result<(), String> {
        if ![self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            || self.x < 0.0
            || self.y < 0.0
            || self.width <= 0.0
            || self.height <= 0.0
        {
            return Err(
                "Browser viewport must have finite, positive dimensions within the app".into(),
            );
        }
        Ok(())
    }

    /// Clips the bounds to a `width` by `height` viewport. Fails if nothing
    /// visible remains.
    pub fn clip(self, width: f64, height: f64) -> Result<Self, String> {
        self.validate()?;
        let clipped = Self {
            width: self.width.min(width - self.x),
            height: self.height.min(height - self.y),
            ..self
        };
        clipped.validate()?;
        Ok(clipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_websites_and_normalizes_host_names() {
        let policy = NavigationPolicy::new(None);
        assert_eq!(
            policy
                .validate("https://EXAMPLE.com/a?q=x#part")
                .unwrap()
                .as_str(),
            "https://example.com/a?q=x#part"
        );
        assert!(policy.validate("http://127.0.0.1:5123/page").is_ok());
    }

    #[test]
    fn denies_non_web_schemes_credentials_and_application_origins() {
        let policy = NavigationPolicy::new(Some(Url::parse("http://localhost:1430").unwrap()));
        for target in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "tauri://localhost",
            "https://user@example.com",
            "https://user:password@example.com",
            "https://tauri.localhost/page",
            "http://ipc.localhost",
            "https://asset.localhost/private",
            "http://localhost:1430/browser.html",
        ] {
            assert!(policy.validate(target).is_err(), "{target}");
        }
        assert!(policy.validate("http://localhost:1431").is_ok());
    }

    #[test]
    fn enforces_url_limit_in_bytes() {
        let policy = NavigationPolicy::new(None);
        let prefix = "https://example.com/";
        assert!(policy
            .validate(&format!("{prefix}{}", "a".repeat(2048 - prefix.len())))
            .is_ok());
        assert!(policy
            .validate(&format!("{prefix}{}", "a".repeat(2049 - prefix.len())))
            .is_err());
        assert!(policy
            .validate(&format!("{prefix}{}", "🦀".repeat(600)))
            .is_err());
    }

    #[test]
    fn enforces_url_limit_after_percent_encoding() {
        let policy = NavigationPolicy::new(None);
        let target = format!("https://example.com/{}", "🦀".repeat(200));
        assert!(target.len() < MAXIMUM_URL_BYTES);
        assert!(policy.validate(&target).is_err());
    }

    #[test]
    fn viewport_rejects_invalid_or_offscreen_rectangles() {
        let bounds = BrowserBounds {
            x: 500.0,
            y: 80.0,
            width: 400.0,
            height: 600.0,
        };
        for invalid in [
            BrowserBounds { x: -1.0, ..bounds },
            BrowserBounds {
                y: f64::NAN,
                ..bounds
            },
            BrowserBounds {
                width: f64::INFINITY,
                ..bounds
            },
            BrowserBounds {
                height: 0.0,
                ..bounds
            },
        ] {
            assert!(invalid.validate().is_err());
        }
        assert!(bounds.clip(500.0, 800.0).is_err());
        assert!(bounds.clip(1200.0, 80.0).is_err());
    }

    #[test]
    fn viewport_clips_to_main_webview() {
        let bounds = BrowserBounds {
            x: 500.0,
            y: 80.0,
            width: 400.0,
            height: 600.0,
        };
        assert_eq!(
            bounds.clip(800.0, 600.0).unwrap(),
            BrowserBounds {
                width: 300.0,
                height: 520.0,
                ..bounds
            }
        );
        assert_eq!(bounds.clip(1200.0, 800.0).unwrap(), bounds);
    }
}
