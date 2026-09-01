//! Pure geometry for the preview-mode launcher slide.
//!
//! While preview mode is active the launcher slides out of the
//! preview's way; on preview exit the exact pre-slide position is
//! restored (`LauncherController::set_preview_mode_active`). Where
//! it slides depends on `[gui] preview_placement`:
//!
//! * [`SlideMode::WindowManaged`] (`"window"`, the default — also
//!   the runtime fallback when `"overlay"` is configured but the
//!   compositor lacks layer-shell): the preview is a normal
//!   WM-managed xdg-toplevel whose position the client cannot know,
//!   so the launcher simply tucks against the LEFT screen edge —
//!   with the typical centered WM placement that minimises the
//!   initial overlap, and any residue is user-recoverable by
//!   dragging the preview.
//! * [`SlideMode::OverlayColumn`] (`"overlay"` with layer-shell):
//!   the preview process maps a surface on the same `Overlay` layer
//!   as the launcher, anchored to the RIGHT monitor edge with a
//!   small margin (see `lixun-preview-bin`'s window setup), and the
//!   launcher centers itself in the remaining LEFT column so both
//!   surfaces are fully visible side by side.
//!
//! Everything here is integer math on logical pixels, kept free of
//! GTK types so it stays unit-testable headlessly. The constants
//! that describe the preview surface mirror `lixun-preview-bin` —
//! the two processes never exchange layout decisions, they only
//! agree on the same arithmetic:
//!
//! * [`PREVIEW_EDGE_MARGIN`] mirrors the preview's right-edge
//!   layer-shell margin.
//! * [`PREVIEW_MIN_WIDTH`] mirrors the preview's `MIN_WIDTH` floor
//!   in `apply_monitor_and_cap`.
//!
//! The width percent / max-px caps are NOT mirrored: both processes
//! read them from the same `[gui]` section of the shared daemon
//! config (`preview_width_percent`, `preview_max_width_px`).

/// Gap the preview keeps between its surface and the right monitor
/// edge. Mirrors `PREVIEW_EDGE_MARGIN` in
/// `crates/lixun-preview-bin/src/main.rs`.
pub(crate) const PREVIEW_EDGE_MARGIN: i32 = 16;

/// Lower bound the preview process applies to its own width.
/// Mirrors `MIN_WIDTH` in `crates/lixun-preview-bin/src/main.rs`.
pub(crate) const PREVIEW_MIN_WIDTH: i32 = 600;

/// Smallest launcher width worth keeping on screen. Below this the
/// results list truncates so badly that soft-hiding the launcher
/// (the preview-side fallback) is the better experience.
pub(crate) const LAUNCHER_MIN_WIDTH: i32 = 480;

/// Breathing room required on each side of the launcher inside the
/// left column before we consider the side-by-side layout viable.
pub(crate) const LAUNCHER_COLUMN_GAP: i32 = 16;

/// Margin from the left screen edge the launcher keeps in
/// [`SlideMode::WindowManaged`] preview mode.
pub(crate) const WINDOW_MODE_EDGE_MARGIN: i32 = 16;

/// Which slide target applies while preview mode is active. Resolved
/// once at window build from `[gui] preview_placement` and runtime
/// layer-shell support (`build_window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlideMode {
    /// `preview_placement = "window"` (default), or `"overlay"`
    /// without runtime layer-shell support: the WM owns the preview's
    /// placement, so the launcher tucks against the left edge.
    WindowManaged,
    /// `preview_placement = "overlay"` with layer-shell support: the
    /// preview anchors to the right edge on the Overlay layer and the
    /// launcher centers in the remaining left column.
    OverlayColumn,
}

/// Snapshot of the launcher's layer-shell position taken before the
/// preview-mode slide, restored verbatim on preview exit. Never
/// written to the persistent per-monitor position store — the slide
/// is a transient layout, not a user preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SavedSlidePosition {
    /// Whether `Edge::Left` was anchored (pinned position) or not
    /// (compositor-centered default).
    pub(crate) left_anchored: bool,
    /// `Edge::Left` margin at slide time (meaningful only when
    /// `left_anchored`; preserved verbatim either way).
    pub(crate) left: i32,
    /// `Edge::Top` margin at slide time.
    pub(crate) top: i32,
}

/// Width the preview surface will take on a monitor of `monitor_w`
/// logical pixels. Must stay arithmetic-identical to the width leg
/// of `apply_monitor_and_cap` in `lixun-preview-bin` (integer
/// percent scaling, capped by `max_px`, floored at
/// [`PREVIEW_MIN_WIDTH`]) so the launcher predicts the same number
/// of pixels the preview actually claims.
pub(crate) fn predicted_preview_width(monitor_w: i32, percent: u8, max_px: i32) -> i32 {
    (monitor_w * i32::from(percent) / 100)
        .min(max_px)
        .max(PREVIEW_MIN_WIDTH)
}

/// Width of the column left of a right-anchored preview surface:
/// everything between the left monitor edge and the preview's left
/// edge (which sits `PREVIEW_EDGE_MARGIN` short of the right edge).
pub(crate) fn left_column_width(monitor_w: i32, preview_w: i32) -> i32 {
    monitor_w - preview_w - PREVIEW_EDGE_MARGIN
}

/// Can a launcher `launcher_w` wide live inside a `column_w` wide
/// column with [`LAUNCHER_COLUMN_GAP`] breathing room on each side?
/// A reported launcher width below [`LAUNCHER_MIN_WIDTH`] (or a
/// degenerate `<= 0` measurement) is clamped up to the floor so a
/// bogus small reading cannot approve a column no real launcher
/// fits in.
pub(crate) fn launcher_fits_column(column_w: i32, launcher_w: i32) -> bool {
    column_w >= launcher_w.max(LAUNCHER_MIN_WIDTH) + 2 * LAUNCHER_COLUMN_GAP
}

/// The `Edge::Left` margin the launcher slides to while preview mode
/// is active, or `None` when it should not slide at all.
///
/// * [`SlideMode::WindowManaged`]: always the fixed
///   [`WINDOW_MODE_EDGE_MARGIN`] left-edge tuck — the WM places the
///   preview, so there is nothing to fit against and the slide never
///   needs skipping (only degenerate measurements bail).
/// * [`SlideMode::OverlayColumn`]: centered within the left column
///   beside a `preview_w` wide right-anchored preview — or `None`
///   when the column is too narrow and the slide must be skipped
///   (the preview process then soft-hides the launcher instead; it
///   applies the same fits predicate).
pub(crate) fn slide_left_margin(
    mode: SlideMode,
    monitor_w: i32,
    launcher_w: i32,
    preview_w: i32,
) -> Option<i32> {
    if monitor_w <= 0 || launcher_w <= 0 {
        return None;
    }
    match mode {
        SlideMode::WindowManaged => Some(WINDOW_MODE_EDGE_MARGIN),
        SlideMode::OverlayColumn => {
            if preview_w <= 0 {
                return None;
            }
            let column_w = left_column_width(monitor_w, preview_w);
            if !launcher_fits_column(column_w, launcher_w) {
                return None;
            }
            Some(((column_w - launcher_w) / 2).max(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicted_width_matches_preview_bin_formula() {
        // 50% of 1920 = 960, under the 1400 cap, over the 600 floor.
        assert_eq!(predicted_preview_width(1920, 50, 1400), 960);
        // Cap binds on wide monitors: 50% of 3840 = 1920 → 1400.
        assert_eq!(predicted_preview_width(3840, 50, 1400), 1400);
        // Floor binds on narrow monitors: 50% of 1000 = 500 → 600.
        assert_eq!(predicted_preview_width(1000, 50, 1400), 600);
        // Integer (truncating) percent scaling, same as preview-bin:
        // 1366 * 50 / 100 = 683.
        assert_eq!(predicted_preview_width(1366, 50, 1400), 683);
    }

    #[test]
    fn column_width_accounts_for_preview_and_its_edge_margin() {
        assert_eq!(left_column_width(1920, 960), 1920 - 960 - PREVIEW_EDGE_MARGIN);
    }

    #[test]
    fn fits_needs_gap_on_both_sides() {
        // Exactly launcher + two gaps: fits.
        assert!(launcher_fits_column(720 + 2 * LAUNCHER_COLUMN_GAP, 720));
        // One pixel short: does not fit.
        assert!(!launcher_fits_column(720 + 2 * LAUNCHER_COLUMN_GAP - 1, 720));
    }

    #[test]
    fn fits_floors_small_or_bogus_launcher_widths() {
        // A 100px "launcher" reading is clamped to the 480 floor.
        assert!(!launcher_fits_column(300, 100));
        assert!(launcher_fits_column(
            LAUNCHER_MIN_WIDTH + 2 * LAUNCHER_COLUMN_GAP,
            100
        ));
        // Degenerate measurement clamps too.
        assert!(launcher_fits_column(
            LAUNCHER_MIN_WIDTH + 2 * LAUNCHER_COLUMN_GAP,
            0
        ));
    }

    #[test]
    fn overlay_slide_centers_launcher_in_left_column() {
        // 1920 monitor, 960 preview → column = 944; launcher 720 →
        // (944 - 720) / 2 = 112.
        assert_eq!(
            slide_left_margin(SlideMode::OverlayColumn, 1920, 720, 960),
            Some(112)
        );
    }

    #[test]
    fn overlay_slide_none_when_column_too_narrow() {
        // 1366 monitor, 683 preview → column = 667; launcher 720
        // needs 752 → no slide (preview side soft-hides instead).
        assert_eq!(
            slide_left_margin(SlideMode::OverlayColumn, 1366, 720, 683),
            None
        );
    }

    #[test]
    fn overlay_slide_rejects_degenerate_inputs() {
        assert_eq!(slide_left_margin(SlideMode::OverlayColumn, 0, 720, 960), None);
        assert_eq!(slide_left_margin(SlideMode::OverlayColumn, 1920, 0, 960), None);
        assert_eq!(slide_left_margin(SlideMode::OverlayColumn, 1920, 720, 0), None);
    }

    #[test]
    fn overlay_slide_never_negative() {
        // Column barely fits the launcher (equality case): margin is
        // the half-gap, never below zero.
        let launcher = 720;
        let column = launcher + 2 * LAUNCHER_COLUMN_GAP;
        let monitor = column + 960 + PREVIEW_EDGE_MARGIN;
        assert_eq!(
            slide_left_margin(SlideMode::OverlayColumn, monitor, launcher, 960),
            Some(LAUNCHER_COLUMN_GAP)
        );
    }

    #[test]
    fn typical_ultrawide_overlay_slides_comfortably() {
        // 3440x1440 ultrawide, capped preview 1400 → column 2024,
        // launcher 720 centered at (2024 - 720) / 2 = 652.
        let pw = predicted_preview_width(3440, 50, 1400);
        assert_eq!(pw, 1400);
        assert_eq!(
            slide_left_margin(SlideMode::OverlayColumn, 3440, 720, pw),
            Some(652)
        );
    }

    #[test]
    fn window_slide_is_the_left_edge_tuck() {
        // WM-managed preview: fixed left-edge margin, independent of
        // preview width — even a "too narrow" overlay column slides.
        assert_eq!(
            slide_left_margin(SlideMode::WindowManaged, 1920, 720, 960),
            Some(WINDOW_MODE_EDGE_MARGIN)
        );
        assert_eq!(
            slide_left_margin(SlideMode::WindowManaged, 1366, 720, 683),
            Some(WINDOW_MODE_EDGE_MARGIN)
        );
        // Unknown/degenerate preview width is irrelevant in this mode.
        assert_eq!(
            slide_left_margin(SlideMode::WindowManaged, 1920, 720, 0),
            Some(WINDOW_MODE_EDGE_MARGIN)
        );
    }

    #[test]
    fn window_slide_rejects_degenerate_monitor_or_launcher() {
        assert_eq!(slide_left_margin(SlideMode::WindowManaged, 0, 720, 960), None);
        assert_eq!(slide_left_margin(SlideMode::WindowManaged, 1920, 0, 960), None);
    }
}
