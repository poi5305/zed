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
    let now = IDLE_AFTER_INPUT_MS * 2.;
    assert_eq!(idle_frame_delay(now, last_input, now - 30.), Some(220.));
    assert_eq!(idle_frame_delay(now, last_input, now - 250.), None);
    assert_eq!(idle_frame_delay(now, now - 10., now - 5.), None);
}

#[test]
fn input_within_five_seconds_is_not_throttled() {
    let now = 1_000_000.;
    assert_eq!(idle_frame_delay(now, now, now), None);
    assert_eq!(idle_frame_delay(now, now - 5_000., now - 1.), None);
    assert!(!user_is_idle(now, now - 5_000.));
    assert!(user_is_idle(now, now - 5_001.));
}

#[test]
fn idle_between_five_and_thirty_seconds_runs_at_four_frames_per_second() {
    let last_input = 1_000_000.;
    for idle_for in [5_001., 10_000., 29_999., 30_000.] {
        let now = last_input + idle_for;
        assert_eq!(
            idle_frame_delay(now, last_input, now),
            Some(250.),
            "idle for {idle_for} ms"
        );
        assert_eq!(
            idle_frame_delay(now, last_input, now - 100.),
            Some(150.),
            "idle for {idle_for} ms"
        );
        assert_eq!(
            idle_frame_delay(now, last_input, now - 250.),
            None,
            "idle for {idle_for} ms"
        );
    }
}

#[test]
fn idle_beyond_thirty_seconds_runs_at_one_frame_per_second() {
    let last_input = 1_000_000.;
    for idle_for in [30_001., 60_000., 3_600_000.] {
        let now = last_input + idle_for;
        assert_eq!(
            idle_frame_delay(now, last_input, now - 250.),
            Some(750.),
            "idle for {idle_for} ms"
        );
        assert_eq!(
            idle_frame_delay(now, last_input, now - 999.),
            Some(1.),
            "idle for {idle_for} ms"
        );
        assert_eq!(
            idle_frame_delay(now, last_input, now - 1_000.),
            None,
            "idle for {idle_for} ms"
        );
    }
}

#[test]
fn crossing_into_the_deeper_tier_never_waits_longer_than_its_interval() {
    let last_input = 1_000_000.;
    for idle_for in [5_001., 20_000., 30_000., 30_001., 45_000.] {
        let now = last_input + idle_for;
        for since_frame in [0., 1., 100., 249., 250., 500., 999., 1_000., 5_000.] {
            if let Some(delay) = idle_frame_delay(now, last_input, now - since_frame) {
                assert!(
                    delay > 0. && since_frame + delay <= 1_000.,
                    "idle {idle_for} ms, last frame {since_frame} ms ago: waits {delay} ms, so the frame lands {} ms after the previous one",
                    since_frame + delay
                );
            }
        }
    }
}

#[test]
fn a_clock_that_moved_backwards_does_not_stall_deeply_idle_frames() {
    let now = 1_000_000.;
    let last_input = now - 60_000.;
    assert_eq!(idle_frame_delay(now, last_input, now + 3_600_000.), None);
    assert_eq!(idle_frame_delay(now, now + 3_600_000., now - 10.), None);
}

#[test]
fn input_after_a_long_idle_restores_unthrottled_frames() {
    let last_input = 1_000_000.;
    let now = last_input + 120_000.;
    assert_eq!(idle_frame_delay(now, last_input, now - 10.), Some(990.));
    let input_at = now;
    assert!(!user_is_idle(now + 16., input_at));
    assert_eq!(idle_frame_delay(now + 16., input_at, now), None);
    assert_eq!(idle_frame_delay(now + 5_000., input_at, now + 4_990.), None);
    assert_eq!(
        idle_frame_delay(now + 5_001., input_at, now + 5_000.),
        Some(249.)
    );
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

#[test]
fn input_during_a_pending_idle_wake_wakes_the_frame_loop() {
    let start = 1_700_000_000_000.;
    let mut frame_loop = FrameLoop::new(start);
    frame_loop.note_input(start);
    let idle = start + 60_000.;
    assert_eq!(
        frame_loop.on_animation_frame(idle),
        AnimationFrameStep::Render
    );
    assert_eq!(
        frame_loop.on_animation_frame(idle + 16.),
        AnimationFrameStep::ScheduleIdleWake(984.)
    );
    // Something dirtied the window while the idle wake was pending: gpui wakes
    // the platform once, on that transition, and the wake defers to the timer.
    assert!(!frame_loop.wake_requests_frame(idle + 100.));
    // The keystroke dirties an already dirty window, so gpui does not wake the
    // platform again; only the input handler can arm a frame for it.
    let must_wake = frame_loop.note_input(idle + 200.);
    assert!(
        must_wake,
        "input at +200 ms left its frame to the idle wake due at +1000 ms: note_input returned {must_wake}, expected true"
    );
    assert!(frame_loop.wake_requests_frame(idle + 200.));
    assert_eq!(
        frame_loop.on_animation_frame(idle + 216.),
        AnimationFrameStep::Render
    );
}

#[test]
fn a_freshly_loaded_page_is_not_idle() {
    let load = 1_700_000_000_000.;
    let mut frame_loop = FrameLoop::new(load);
    assert_eq!(
        frame_loop.on_animation_frame(load),
        AnimationFrameStep::Render
    );
    assert_eq!(
        frame_loop.on_animation_frame(load + 16.),
        AnimationFrameStep::Render,
        "the second frame after load was throttled"
    );
    let idle = load + IDLE_AFTER_INPUT_MS + 16.;
    assert_eq!(
        frame_loop.on_animation_frame(idle),
        AnimationFrameStep::Render
    );
    assert_eq!(
        frame_loop.on_animation_frame(idle + 16.),
        AnimationFrameStep::ScheduleIdleWake(IDLE_FRAME_INTERVAL_MS - 16.)
    );
}
