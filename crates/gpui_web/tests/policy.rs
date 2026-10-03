//! Host tests for `src/policy.rs`: the crate is wasm-only, so the pure module
//! is compiled directly into this test target.
#![allow(dead_code)]

#[path = "../src/policy.rs"]
mod policy;

use policy::*;

/// `ScrollPhysics::ios()` fling duration in ms (gpui/src/gestures.rs).
fn ios_fling_duration_ms(speed: f32) -> f64 {
    ((10.0_f64 / speed as f64).ln() / 0.998_f64.ln()).max(0.)
}

#[test]
fn idle_threshold_outlasts_a_fast_touch_fling() {
    let duration = ios_fling_duration_ms(10_000.);
    assert!(
        IDLE_AFTER_INPUT_MS > duration,
        "idle after {IDLE_AFTER_INPUT_MS} ms but a 10000 px/s fling coasts {duration:.0} ms, so its tail would run at 10 fps"
    );
}

#[test]
fn a_fling_tail_is_not_deferred() {
    let fling_end = ios_fling_duration_ms(10_000.);
    let last_input = 1_000_000.;
    let now = last_input + fling_end - 10.;
    assert_eq!(idle_frame_delay(now, last_input, now - 16.), None);
}

#[test]
fn idle_page_defers_a_frame_that_is_too_soon_and_renders_one_that_is_due() {
    let last_input = 0.;
    let now = IDLE_AFTER_INPUT_MS * 10.;
    assert_eq!(idle_frame_delay(now, last_input, now - 30.), Some(70.));
    assert_eq!(idle_frame_delay(now, last_input, now - 100.), None);
    assert_eq!(idle_frame_delay(now, now - 10., now - 5.), None);
}

#[test]
fn a_clock_that_moved_backwards_does_not_stall_idle_frames() {
    let now = 1_000_000.;
    let last_frame_in_the_future = now + 3_600_000.;
    let delay = idle_frame_delay(now, 0., last_frame_in_the_future);
    assert_eq!(
        delay, None,
        "frame deferred for {delay:?} ms after the clock stepped back an hour"
    );
}

#[test]
fn raw_zhuyin_and_kana_letters_are_detached_ime_keys() {
    for key in ["ㄅ", "ㄓ", "ˊ", "ˇ", "ˋ", "˙", "あ", "ア", "ㅂ", "ᄀ"] {
        assert!(is_detached_ime_key(key, false), "{key:?}");
    }
    for key in ["a", "A", "Enter", "ab", "", "1", "。", "あい"] {
        assert!(!is_detached_ime_key(key, false), "{key:?}");
    }
}

#[test]
fn option_combinations_on_an_english_layout_are_not_detached_ime_keys() {
    // Option+H and Option+Shift+T type these characters on a US layout.
    for key in ["\u{02D9}", "\u{02C7}"] {
        assert!(
            !is_detached_ime_key(key, true),
            "{key:?} with Alt would be dropped and refocus the input"
        );
    }
}

#[test]
fn directly_typed_japanese_marks_are_not_detached_ime_keys() {
    for key in [
        "\u{30FB}", "\u{30FC}", "\u{30A0}", "\u{309B}", "\u{3099}", "\u{3040}",
    ] {
        assert!(
            !is_detached_ime_key(key, false),
            "{key:?} is not a kana letter"
        );
    }
}

#[test]
fn late_text_paste_matching_the_flushed_text_is_a_repeat() {
    assert!(late_paste_repeats_keystroke(
        5.,
        1000.,
        Some("a"),
        Some("a"),
        false
    ));
    assert!(!late_paste_repeats_keystroke(
        5.,
        1000.,
        Some("a"),
        Some("b"),
        false
    ));
    assert!(!late_paste_repeats_keystroke(
        2000.,
        1000.,
        Some("a"),
        Some("a"),
        true
    ));
}

#[test]
fn late_image_paste_matching_the_flushed_item_is_a_repeat() {
    assert!(
        late_paste_repeats_keystroke(5., 1000., None, None, true),
        "the same image pasted twice"
    );
    assert!(!late_paste_repeats_keystroke(5., 1000., None, None, false));
    assert!(!late_paste_repeats_keystroke(
        5.,
        1000.,
        Some("a"),
        None,
        true
    ));
}
