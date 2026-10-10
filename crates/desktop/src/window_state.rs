//! Restores a saved window's geometry only where its header stays reachable.
//!
//! The saved file is the one tauri-plugin-window-state 2.4.1 writes. This
//! module only reads it: a front end decides which monitors and offsets to
//! pass in and applies the result to its own window.
use serde::Deserialize;
use std::collections::HashMap;

/// A physical-pixel point, such as a window or monitor origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// Horizontal coordinate; negative left of the primary monitor.
    pub x: i32,
    /// Vertical coordinate; negative above the primary monitor.
    pub y: i32,
}

impl Position {
    /// Creates a position from its coordinates.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A physical-pixel size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

impl Size {
    /// Creates a size from its dimensions.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// A physical-pixel rectangle, such as a monitor's work area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Top-left corner.
    pub position: Position,
    /// Extent from the top-left corner.
    pub size: Size,
}

/// One window's entry in window-state 2.4.1's file: position is the outer
/// origin, size is the client size, and maximized windows use prev_x/prev_y.
// Keep all required fields typed so corrupt state falls back just as it does
// in the plugin. The plugin remains the only writer and mode/lifecycle owner.
#[derive(Deserialize)]
pub struct SavedGeometry {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    prev_x: i32,
    prev_y: i32,
    maximized: bool,
    #[serde(rename = "visible")]
    _visible: bool,
    #[serde(rename = "decorated")]
    _decorated: bool,
    #[serde(rename = "fullscreen")]
    _fullscreen: bool,
}

/// Reads `label`'s entry from the saved file's contents. Returns `None` for a
/// missing, corrupt or empty entry, so the window keeps its default geometry.
pub fn saved_geometry(bytes: &[u8], label: &str) -> Option<SavedGeometry> {
    serde_json::from_slice::<HashMap<String, SavedGeometry>>(bytes)
        .ok()?
        .remove(label)
        .filter(|state| state.width > 0 && state.height > 0)
}

/// Plans the outer position and client size to request for `saved`.
///
/// `client_offset` is the client origin minus the frame origin, and
/// `header_height` the app header in physical pixels. If the header would
/// not be reachable on any of `areas`, the window moves to the top of
/// `fallback`, shrunk to fit. Plan before setting anything: native setters
/// can apply asynchronously, so current window bounds may be stale.
pub fn restore_request(
    saved: &SavedGeometry,
    client_offset: Position,
    header_height: u32,
    areas: &[Rect],
    fallback: Rect,
) -> (Position, Size) {
    let position = if saved.maximized {
        Position::new(saved.prev_x, saved.prev_y)
    } else {
        Position::new(saved.x, saved.y)
    };
    let client = Rect {
        position: Position::new(
            position.x.saturating_add(client_offset.x),
            position.y.saturating_add(client_offset.y),
        ),
        size: Size::new(saved.width, saved.height),
    };
    let target = reachable_client(client, header_height, areas, fallback).unwrap_or(client);
    (
        Position::new(
            target.position.x.saturating_sub(client_offset.x),
            target.position.y.saturating_sub(client_offset.y),
        ),
        target.size,
    )
}

fn reachable_client(
    client: Rect,
    header_height: u32,
    areas: &[Rect],
    fallback: Rect,
) -> Option<Rect> {
    let x = i64::from(client.position.x);
    let y = i64::from(client.position.y);
    let right = x + i64::from(client.size.width.saturating_sub(1));
    let bottom = y + i64::from(header_height.saturating_sub(1));
    // A header may span adjacent monitors. Both control regions must remain
    // reachable, but need not belong to the same monitor.
    let reachable = [(x, y), (right, y), (x, bottom), (right, bottom)]
        .into_iter()
        .all(|(x, y)| {
            areas.iter().any(|area| {
                x >= i64::from(area.position.x)
                    && y >= i64::from(area.position.y)
                    && x < i64::from(area.position.x) + i64::from(area.size.width)
                    && y < i64::from(area.position.y) + i64::from(area.size.height)
            })
        });
    if reachable {
        return None;
    }
    // Unlike any-corner intersection, this keeps the drag strip and both sets
    // of platform controls on-screen, including after removing an upper monitor.
    let size = Size::new(
        client.size.width.min(fallback.size.width),
        client.size.height.min(fallback.size.height),
    );
    Some(Rect {
        position: Position::new(
            fallback
                .position
                .x
                .saturating_add(((fallback.size.width - size.width) / 2) as i32),
            fallback.position.y,
        ),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::{Position, Rect, Size};
    use serde_json::{json, Value};

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            position: Position::new(x, y),
            size: Size::new(width, height),
        }
    }

    fn assert_frame(actual: Option<Rect>, expected: Option<Rect>) {
        assert_eq!(actual, expected);
    }

    #[test]
    fn partial_overlap_after_removing_upper_monitor_recovers_entire_header() {
        let laptop = rect(0, 25, 1440, 875);
        let saved = rect(100, -450, 960, 640);
        assert_frame(
            super::reachable_client(saved, 48, &[laptop], laptop),
            Some(rect(240, 25, 960, 640)),
        );
        let upper = rect(0, -900, 1440, 900);
        assert_frame(
            super::reachable_client(saved, 48, &[laptop, upper], laptop),
            None,
        );
    }

    #[test]
    fn reachable_geometry_is_unchanged_including_negative_coordinates() {
        let primary = rect(0, 25, 1440, 875);
        let left = rect(-1920, 0, 1920, 1080);
        for saved in [rect(100, 120, 960, 640), rect(-1800, 80, 960, 640)] {
            assert_frame(
                super::reachable_client(saved, 48, &[primary, left], primary),
                None,
            );
        }
    }

    #[test]
    fn side_overlap_and_menu_bar_occlusion_recover_controls_not_just_a_corner() {
        let area = rect(0, 25, 1440, 875);
        for saved in [
            rect(-600, 100, 960, 640),
            rect(1300, 100, 960, 640),
            rect(100, 0, 960, 640),
            rect(100, 860, 960, 640),
        ] {
            assert_frame(
                super::reachable_client(saved, 48, &[area], area),
                Some(rect(240, 25, 960, 640)),
            );
        }
    }

    #[test]
    fn oversized_window_fits_remaining_work_area_and_scaled_header_is_checked() {
        let area = rect(0, 50, 1440, 850);
        assert_frame(
            super::reachable_client(rect(0, 50, 2800, 1800), 96, &[area], area),
            Some(area),
        );
        assert_frame(
            super::reachable_client(rect(100, 820, 960, 640), 48, &[area], area),
            None,
        );
        assert_frame(
            super::reachable_client(rect(100, 820, 960, 640), 96, &[area], area),
            Some(rect(240, 50, 960, 640)),
        );
    }

    #[test]
    fn invisible_windows_frame_border_does_not_displace_edge_flush_client() {
        let area = rect(0, 0, 1920, 1040);
        let saved = decode(&saved_state(-7, 0, 1920, 1040));
        assert_eq!(
            super::restore_request(&saved, Position::new(7, 0), 48, &[area], area),
            (Position::new(-7, 0), area.size),
        );
    }

    #[test]
    fn header_can_span_adjacent_monitors_but_not_extend_above_either() {
        let left = rect(-1920, 0, 1920, 1080);
        let right = rect(0, 0, 1920, 1080);
        let spanning = rect(-480, 100, 960, 640);
        assert_frame(
            super::reachable_client(spanning, 48, &[left, right], right),
            None,
        );
        let below = rect(0, 300, 1920, 1080);
        assert_frame(
            super::reachable_client(spanning, 48, &[left, below], left),
            Some(rect(-1440, 0, 960, 640)),
        );
    }

    fn saved_state(x: i32, y: i32, width: u32, height: u32) -> Value {
        json!({"main": {
            "width": width, "height": height, "x": x, "y": y,
            "prev_x": x, "prev_y": y, "maximized": false,
            "fullscreen": false, "visible": false, "decorated": false
        }})
    }

    fn decode(state: &Value) -> super::SavedGeometry {
        super::saved_geometry(&serde_json::to_vec(state).unwrap(), "main").unwrap()
    }

    #[test]
    fn requested_geometry_is_safe_even_while_getters_still_report_startup_bounds() {
        let area = rect(0, 25, 1440, 875);
        let startup = rect(100, 120, 1200, 800);
        let completed = rect(100, -450, 960, 640);
        let saved = decode(&saved_state(100, -450, 960, 640));
        // The old post-set guard approves this stale, reachable startup frame.
        assert_frame(super::reachable_client(startup, 48, &[area], area), None);
        // The planner accepts no current bounds: they can be either startup or
        // completed bounds. Only the frame-to-client offset (zero for this
        // undecorated window) is used. The unsafe origin is never sent to GTK.
        let expected = (Position::new(240, 25), Size::new(960, 640));
        assert_frame(
            super::reachable_client(completed, 48, &[area], area),
            Some(rect(240, 25, 960, 640)),
        );
        let offset = Position::new(0, 0);
        assert_eq!(
            super::restore_request(&saved, offset, 48, &[area], area),
            expected
        );
        // Resize is asynchronous too: validating only startup width misses
        // the right-hand controls of a wider saved client.
        let saved = decode(&saved_state(100, 120, 1800, 800));
        assert_eq!(
            super::restore_request(&saved, offset, 48, &[area], area),
            (area.position, Size::new(1440, 800)),
        );
    }

    #[test]
    fn requested_geometry_uses_client_size_and_normal_origin_for_saved_modes() {
        let area = rect(0, 0, 1920, 1040);
        let offset = Position::new(7, 0);
        let saved = decode(&saved_state(-7, 0, 1920, 1000));
        assert_eq!(
            super::restore_request(&saved, offset, 48, &[area], area),
            (Position::new(-7, 0), Size::new(1920, 1000)),
        );
        let mut state = saved_state(0, 0, 960, 640);
        state["main"]["maximized"] = json!(true);
        state["main"]["prev_x"] = json!(100);
        state["main"]["prev_y"] = json!(-450);
        assert_eq!(
            super::restore_request(&decode(&state), offset, 48, &[area], area),
            (Position::new(473, 0), Size::new(960, 640)),
        );
        // Fullscreen uses x/y, not prev_x/prev_y, matching the plugin format.
        state["main"]["maximized"] = json!(false);
        state["main"]["fullscreen"] = json!(true);
        assert_eq!(
            super::restore_request(&decode(&state), offset, 48, &[area], area),
            (Position::new(0, 0), Size::new(960, 640)),
        );
    }

    #[test]
    fn saved_geometry_matches_plugin_schema_and_rejects_invalid_snapshots() {
        let valid = saved_state(100, 120, 960, 640);
        let mut missing_mode = valid.clone();
        missing_mode["main"]
            .as_object_mut()
            .unwrap()
            .remove("fullscreen");
        let mut invalid_other_window = valid.clone();
        invalid_other_window["other"] = json!({"width": 960});
        let mut wrong_type = valid.clone();
        wrong_type["main"]["maximized"] = json!("false");
        for state in [
            json!({}),
            json!(null),
            missing_mode,
            invalid_other_window,
            wrong_type,
            saved_state(0, 0, 0, 0),
        ] {
            assert!(super::saved_geometry(&serde_json::to_vec(&state).unwrap(), "main").is_none());
        }
        assert!(super::saved_geometry(b"not json", "main").is_none());
        assert!(super::saved_geometry(&serde_json::to_vec(&valid).unwrap(), "other").is_none());
        assert!(super::saved_geometry(&serde_json::to_vec(&valid).unwrap(), "main").is_some());
    }
}
