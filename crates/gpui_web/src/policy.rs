//! Decisions the web platform makes from plain values, kept free of `web_sys`
//! so they can be unit-tested on the host (the crate itself is wasm-only).

/// How long after the last user input the page counts as idle. A touch fling
/// keeps scrolling by animation frames long after the finger left, up to
/// about 3.5 s for a fast flick, so the page must not count as idle before
/// that.
pub(crate) const IDLE_AFTER_INPUT_MS: f64 = 5000.;
/// The minimum time between frames while idle: 4 frames per second.
pub(crate) const IDLE_FRAME_INTERVAL_MS: f64 = 250.;
/// How long after the last user input the page counts as deeply idle: nobody
/// is looking at it, so only looping animations such as a status dot still run.
pub(crate) const DEEP_IDLE_AFTER_INPUT_MS: f64 = 30_000.;
/// The minimum time between frames while deeply idle: 1 frame per second.
pub(crate) const DEEP_IDLE_FRAME_INTERVAL_MS: f64 = 1000.;

pub(crate) fn user_is_idle(now: f64, last_input_at: f64) -> bool {
    now - last_input_at > IDLE_AFTER_INPUT_MS
}

/// The minimum time between frames now, or `None` while the user is active.
fn idle_frame_interval(now: f64, last_input_at: f64) -> Option<f64> {
    if !user_is_idle(now, last_input_at) {
        None
    } else if now - last_input_at > DEEP_IDLE_AFTER_INPUT_MS {
        Some(DEEP_IDLE_FRAME_INTERVAL_MS)
    } else {
        Some(IDLE_FRAME_INTERVAL_MS)
    }
}

/// How long to defer the frame that is due now, or `None` to render it. The
/// delay is measured from the last frame against the interval of the tier in
/// force now, so a tier change never stretches the wait past the new interval.
pub(crate) fn idle_frame_delay(now: f64, last_input_at: f64, last_frame_at: f64) -> Option<f64> {
    let interval = idle_frame_interval(now, last_input_at)?;
    let since_frame = now - last_frame_at;
    // A negative interval means the wall clock stepped backwards; the frame is
    // overdue then, not due in an hour.
    if (0.0..interval).contains(&since_frame) {
        Some(interval - since_frame)
    } else {
        None
    }
}

/// Whether a keydown is a script key that only an IME produces, arriving raw
/// because iPadOS detached the IME from the hidden input. Option
/// combinations on an English layout type U+02D9 and U+02C7 directly, and the
/// katakana middle dot and prolonged sound mark are typed directly too, so
/// those never count.
pub(crate) fn is_detached_ime_key(key: &str, alt_key: bool) -> bool {
    if alt_key {
        return false;
    }
    let mut characters = key.chars();
    let (Some(character), None) = (characters.next(), characters.next()) else {
        return false;
    };
    matches!(
        character,
        '\u{3100}'..='\u{312F}'
            | '\u{31A0}'..='\u{31BF}'
            | '\u{02C7}'
            | '\u{02C9}'..='\u{02CB}'
            | '\u{02D9}'
            | '\u{3041}'..='\u{3096}'
            | '\u{309D}'..='\u{309F}'
            | '\u{30A1}'..='\u{30FA}'
            | '\u{30FD}'..='\u{30FF}'
            | '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
    )
}

/// Whether a `paste` event that arrives after its keystroke already ran
/// against the in-app clipboard repeats that paste. Text is compared by
/// content (metadata differs between the in-app copy and the browser's);
/// content without text, such as an image, only by whole-item equality.
pub(crate) fn late_paste_repeats_keystroke(
    age_ms: f64,
    window_ms: f64,
    flushed_text: Option<&str>,
    pasted_text: Option<&str>,
    same_item: bool,
) -> bool {
    age_ms < window_ms
        && match (flushed_text, pasted_text) {
            (Some(flushed), Some(pasted)) => flushed == pasted,
            (None, None) => same_item,
            _ => false,
        }
}
