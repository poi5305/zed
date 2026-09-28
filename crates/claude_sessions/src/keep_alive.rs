//! When to send a short message so a Claude session's one-hour prompt cache does not expire.
//!
//! No GPUI here. The store and the panels call these functions; they do not decide the rules.

use serde::Deserialize;

pub use crate::session_registry::CacheTtl;

pub const DEFAULT_INTERVAL_MINUTES: u32 = 50;
pub const MIN_INTERVAL_MINUTES: u32 = 5;
pub const MAX_INTERVAL_MINUTES: u32 = 55;
pub const DEFAULT_MAX_HOURS: u32 = 12;
pub const MIN_MAX_HOURS: u32 = 1;
pub const MAX_MAX_HOURS: u32 = 48;
/// A bare word rather than an instruction: a ping queued behind a long tool call lands in
/// the middle of a task, and telling Claude to stop there would derail it.
pub const DEFAULT_MESSAGE: &str = "ok";
pub const ONE_HOUR_CACHE_MS: i64 = 3_600_000;
/// How long an unanswered ping is waited for, on Zed's clock, before keep-alive pauses, and how long
/// after a ping every newly observed answer still counts as part of its reply (see `observe_answer`).
pub const PING_REPLY_WINDOW_MS: i64 = 600_000;

const MILLISECONDS_PER_MINUTE: i64 = 60_000;
const MILLISECONDS_PER_HOUR: i64 = 3_600_000;
pub(crate) const TEN_MINUTES_MS: i64 = 600_000;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct KeepAliveConfig {
    pub interval_ms: i64,
    pub max_idle_ms: i64,
    pub message: String,
}

impl KeepAliveConfig {
    /// None -> default. Out-of-range values are clamped into [MIN, MAX]. A message that is empty after trim -> DEFAULT_MESSAGE.
    pub fn new(
        interval_minutes: Option<u32>,
        max_hours: Option<u32>,
        message: Option<String>,
    ) -> Self {
        let interval_minutes = interval_minutes
            .unwrap_or(DEFAULT_INTERVAL_MINUTES)
            .clamp(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES);
        let max_hours = max_hours
            .unwrap_or(DEFAULT_MAX_HOURS)
            .clamp(MIN_MAX_HOURS, MAX_MAX_HOURS);
        let message = match message {
            Some(message) if !message.trim().is_empty() => message,
            _ => DEFAULT_MESSAGE.to_string(),
        };
        Self {
            interval_ms: i64::from(interval_minutes) * MILLISECONDS_PER_MINUTE,
            max_idle_ms: i64::from(max_hours) * MILLISECONDS_PER_HOUR,
            message,
        }
    }
}

impl Default for KeepAliveConfig {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepAliveState {
    pub enabled: bool,
    /// Newest answer that was not the reply to a ping: the idle clock starts here.
    pub last_real_answer_ms: Option<i64>,
    /// Newest answer seen at all (a ping's reply included): the cache was last refreshed here.
    pub last_seen_answer_ms: Option<i64>,
    pub last_ping_ms: Option<i64>,
    /// Pings sent since the last real answer.
    pub pings_sent: u32,
    /// A ping was sent and has not been answered yet: the next newer answer is its reply, however
    /// late it is observed. The answer's timestamp is never compared with the ping's, because those
    /// clocks belong to different machines.
    pub ping_outstanding: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionFacts {
    pub cache_ttl: CacheTtl,
    pub channel_live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    NoAnswerYet,
    CacheTtlNotOneHour,
    ChannelNotLoaded,
    PingUnanswered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepAliveStatus {
    Off,
    Scheduled {
        next_ping_ms: i64,
        cache_expires_ms: i64,
    },
    SendNow,
    AwaitingReply {
        sent_ms: i64,
    },
    Paused(PauseReason),
    LimitReached {
        idle_since_ms: i64,
    },
    CacheExpired {
        expired_ms: i64,
    },
}

/// Feeds the newest answer time the scan saw. Only a strictly newer `Some(answer)` than `last_seen_answer_ms`
/// (or the first one) changes anything: it becomes `last_seen_answer_ms`. It is part of the ping's reply
/// when a ping was sent and either it is still outstanding (however late the reply is observed) or the
/// answer is observed within `PING_REPLY_WINDOW_MS` of it on Zed's clock: then `ping_outstanding = false`
/// and nothing else changes. Otherwise it is a real answer: `last_real_answer_ms = Some(answer)`, `pings_sent = 0`,
/// `last_ping_ms = None`, `ping_outstanding = false`. `None` or an older or equal answer: no change.
///
/// One reply is several usage records seconds apart, often seen across two scans, and may make several
/// API calls; a reply seen late (a slow scan, a reconnect) is still the reply. Counting any of those as
/// real would restart the idle clock and defeat `max_idle_ms`, while a real answer counted as a reply only
/// stops keep-alive sooner.
pub fn observe_answer(state: &mut KeepAliveState, last_answer_ms: Option<i64>, now_ms: i64) {
    let Some(answer) = last_answer_ms else {
        return;
    };
    if state.last_seen_answer_ms.is_some_and(|seen| answer <= seen) {
        return;
    }
    state.last_seen_answer_ms = Some(answer);
    let ping_reply = state.last_ping_ms.is_some_and(|ping| {
        state.ping_outstanding || now_ms.saturating_sub(ping) <= PING_REPLY_WINDOW_MS
    });
    if ping_reply {
        state.ping_outstanding = false;
        return;
    }
    state.last_real_answer_ms = Some(answer);
    state.pings_sent = 0;
    state.last_ping_ms = None;
    state.ping_outstanding = false;
}

/// Turns keep-alive on or off. Turning it on copies `last_seen_answer_ms` into `last_real_answer_ms`
/// when a seen answer exists (always, including when the idle clock was already set), and clears
/// `last_ping_ms`, `pings_sent`, and `ping_outstanding`. Turning off only sets enabled=false.
pub fn set_enabled(state: &mut KeepAliveState, enabled: bool) {
    state.enabled = enabled;
    if !enabled {
        return;
    }
    if state.last_seen_answer_ms.is_some() {
        state.last_real_answer_ms = state.last_seen_answer_ms;
    }
    state.last_ping_ms = None;
    state.pings_sent = 0;
    state.ping_outstanding = false;
}

/// Records that a ping was just sent at `now_ms`: last_ping_ms = Some(now_ms), pings_sent += 1 (saturating),
/// ping_outstanding = true.
pub fn record_ping(state: &mut KeepAliveState, now_ms: i64) {
    state.last_ping_ms = Some(now_ms);
    state.pings_sent = state.pings_sent.saturating_add(1);
    state.ping_outstanding = true;
}

/// Evaluated in this exact order; the first rule that applies wins.
pub fn status(
    state: &KeepAliveState,
    facts: SessionFacts,
    config: &KeepAliveConfig,
    now_ms: i64,
) -> KeepAliveStatus {
    if !state.enabled {
        return KeepAliveStatus::Off;
    }
    let (Some(real), Some(seen)) = (state.last_real_answer_ms, state.last_seen_answer_ms) else {
        return KeepAliveStatus::Paused(PauseReason::NoAnswerYet);
    };
    if facts.cache_ttl != CacheTtl::OneHour {
        return KeepAliveStatus::Paused(PauseReason::CacheTtlNotOneHour);
    }
    if now_ms.saturating_sub(real) >= config.max_idle_ms {
        return KeepAliveStatus::LimitReached {
            idle_since_ms: real,
        };
    }
    // Counted on this machine, so a skewed session clock cannot keep the idle limit from arriving.
    if i64::from(state.pings_sent).saturating_mul(config.interval_ms) >= config.max_idle_ms {
        return KeepAliveStatus::LimitReached {
            idle_since_ms: real,
        };
    }
    // `ping_outstanding` with no ping time cannot happen; treat that as not outstanding.
    if state.ping_outstanding
        && let Some(ping) = state.last_ping_ms
    {
        if now_ms.saturating_sub(ping) <= PING_REPLY_WINDOW_MS {
            return KeepAliveStatus::AwaitingReply { sent_ms: ping };
        }
        return KeepAliveStatus::Paused(PauseReason::PingUnanswered);
    }
    let expires = seen.saturating_add(ONE_HOUR_CACHE_MS);
    if now_ms >= expires {
        return KeepAliveStatus::CacheExpired {
            expired_ms: expires,
        };
    }
    // Neither a busy session nor one waiting for the user pauses the ping: a session left busy
    // by background tasks or subagents makes no API calls of its own while it waits on them,
    // so pausing would let the cache expire under them.
    let next = seen.saturating_add(config.interval_ms);
    if now_ms >= next {
        if facts.channel_live {
            KeepAliveStatus::SendNow
        } else {
            KeepAliveStatus::Paused(PauseReason::ChannelNotLoaded)
        }
    } else {
        KeepAliveStatus::Scheduled {
            next_ping_ms: next,
            cache_expires_ms: expires,
        }
    }
}

/// What one ping and one rewrite would cost, for the tooltip: (context * cache_read, context * cache_write_1h), USD.
pub fn ping_and_rewrite_cost(context_tokens: u64, rates: crate::ModelRates) -> (f64, f64) {
    const PER_MILLION: f64 = 1_000_000.;
    let at = |rate: f64| context_tokens as f64 * rate / PER_MILLION;
    (at(rates.cache_read), at(rates.cache_write_1h))
}

/// "<1m", "Nm" under an hour, "NhMm" ("1h05m") after that. Negative remaining is "0m".
pub fn format_countdown(remaining_ms: i64) -> String {
    if remaining_ms < 0 {
        return "0m".to_string();
    }
    if remaining_ms < MILLISECONDS_PER_MINUTE {
        return "<1m".to_string();
    }
    let minutes_total = remaining_ms / MILLISECONDS_PER_MINUTE;
    if minutes_total < 60 {
        return format!("{minutes_total}m");
    }
    let hours = minutes_total / 60;
    let minutes = minutes_total % 60;
    format!("{hours}h{minutes:02}m")
}

pub(crate) fn pause_reason_text(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::NoAnswerYet => "No answer yet",
        PauseReason::CacheTtlNotOneHour => {
            "This session's cache lives 5 minutes (or the host is too old to say)"
        }
        PauseReason::ChannelNotLoaded => "The Zed channel is not loaded in this session",
        PauseReason::PingUnanswered => "The last ping got no answer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelRates;

    fn ready_state() -> KeepAliveState {
        KeepAliveState {
            enabled: true,
            last_real_answer_ms: Some(0),
            last_seen_answer_ms: Some(0),
            last_ping_ms: None,
            pings_sent: 0,
            ping_outstanding: false,
        }
    }

    fn hour_facts() -> SessionFacts {
        SessionFacts {
            cache_ttl: CacheTtl::OneHour,
            channel_live: true,
        }
    }

    fn default_config() -> KeepAliveConfig {
        KeepAliveConfig::default()
    }

    #[test]
    fn status_is_off_while_keep_alive_is_disabled() {
        let mut state = ready_state();
        state.enabled = false;
        let now = default_config().max_idle_ms;
        assert_eq!(
            status(&state, hour_facts(), &default_config(), now),
            KeepAliveStatus::Off
        );
    }

    #[test]
    fn status_is_paused_when_either_answer_clock_is_missing() {
        let config = default_config();
        let mut facts = hour_facts();
        facts.cache_ttl = CacheTtl::FiveMinutes;
        let mut state = ready_state();
        state.last_real_answer_ms = None;
        assert_eq!(
            status(&state, facts, &config, config.max_idle_ms),
            KeepAliveStatus::Paused(PauseReason::NoAnswerYet)
        );

        state.last_real_answer_ms = Some(1);
        state.last_seen_answer_ms = None;
        assert_eq!(
            status(&state, facts, &config, config.max_idle_ms),
            KeepAliveStatus::Paused(PauseReason::NoAnswerYet)
        );
    }

    #[test]
    fn status_is_paused_unless_the_cache_ttl_is_one_hour() {
        let state = ready_state();
        let config = default_config();
        let now = config.max_idle_ms;
        for cache_ttl in [CacheTtl::FiveMinutes, CacheTtl::Unknown] {
            let facts = SessionFacts {
                cache_ttl,
                ..hour_facts()
            };
            assert_eq!(
                status(&state, facts, &config, now),
                KeepAliveStatus::Paused(PauseReason::CacheTtlNotOneHour),
                "{cache_ttl:?} is not the one-hour cache this keeps warm"
            );
        }
    }

    #[test]
    fn status_reaches_the_idle_limit_from_the_ping_count_alone() {
        let mut state = ready_state();
        // 5 minutes * 12 = 1 hour, the configured idle limit. Wall-clock idle is still zero.
        let config = KeepAliveConfig::new(Some(5), Some(1), None);
        state.pings_sent = 11;
        assert_eq!(
            status(&state, hour_facts(), &config, 0),
            KeepAliveStatus::Scheduled {
                next_ping_ms: config.interval_ms,
                cache_expires_ms: ONE_HOUR_CACHE_MS,
            }
        );
        state.pings_sent = 12;
        assert_eq!(
            status(&state, hour_facts(), &config, 0),
            KeepAliveStatus::LimitReached { idle_since_ms: 0 }
        );
    }

    #[test]
    fn status_stops_once_the_idle_limit_is_reached() {
        let state = ready_state();
        let config = default_config();
        assert_eq!(
            status(&state, hour_facts(), &config, config.max_idle_ms),
            KeepAliveStatus::LimitReached { idle_since_ms: 0 }
        );
    }

    #[test]
    fn status_waits_for_a_ping_reply_and_then_pauses() {
        let mut state = ready_state();
        let seen = 1_000_000;
        state.last_real_answer_ms = Some(seen);
        state.last_seen_answer_ms = Some(seen);
        let ping = seen + 50 * MILLISECONDS_PER_MINUTE;
        state.last_ping_ms = Some(ping);
        state.ping_outstanding = true;
        let config = default_config();

        assert_eq!(
            status(&state, hour_facts(), &config, ping + PING_REPLY_WINDOW_MS),
            KeepAliveStatus::AwaitingReply { sent_ms: ping }
        );
        assert_eq!(
            status(
                &state,
                hour_facts(),
                &config,
                ping + PING_REPLY_WINDOW_MS + 1
            ),
            KeepAliveStatus::Paused(PauseReason::PingUnanswered)
        );
    }

    #[test]
    fn status_reports_a_cache_that_has_already_expired() {
        let state = ready_state();
        assert_eq!(
            status(&state, hour_facts(), &default_config(), ONE_HOUR_CACHE_MS),
            KeepAliveStatus::CacheExpired {
                expired_ms: ONE_HOUR_CACHE_MS
            }
        );
    }

    #[test]
    fn status_sends_schedules_or_waits_for_the_channel() {
        let state = ready_state();
        let config = default_config();
        let facts = hour_facts();
        assert_eq!(
            status(&state, facts, &config, config.interval_ms),
            KeepAliveStatus::SendNow
        );

        let mut channel_down = facts;
        channel_down.channel_live = false;
        assert_eq!(
            status(&state, channel_down, &config, config.interval_ms),
            KeepAliveStatus::Paused(PauseReason::ChannelNotLoaded)
        );

        assert_eq!(
            status(&state, facts, &config, config.interval_ms - 1),
            KeepAliveStatus::Scheduled {
                next_ping_ms: config.interval_ms,
                cache_expires_ms: ONE_HOUR_CACHE_MS,
            }
        );
    }

    #[test]
    fn observe_answer_treats_a_reply_inside_the_window_as_a_ping_and_anything_else_as_real() {
        let mut state = KeepAliveState::default();
        observe_answer(&mut state, None, 0);
        assert_eq!(state, KeepAliveState::default());

        observe_answer(&mut state, Some(1_000), 1_000);
        assert_eq!(state.last_seen_answer_ms, Some(1_000));
        assert_eq!(state.last_real_answer_ms, Some(1_000));
        assert_eq!(state.last_ping_ms, None);
        assert_eq!(state.pings_sent, 0);
        assert!(!state.ping_outstanding);

        record_ping(&mut state, 2_000);
        observe_answer(&mut state, Some(1_000), 2_000);
        assert_eq!(state.last_seen_answer_ms, Some(1_000));
        assert_eq!(state.last_ping_ms, Some(2_000));
        assert_eq!(state.pings_sent, 1);
        assert!(state.ping_outstanding);

        // The window is Zed's clock (`now_ms - ping`), not the answer timestamp.
        let reply = 2_000 + PING_REPLY_WINDOW_MS;
        observe_answer(&mut state, Some(reply), reply);
        assert_eq!(state.last_seen_answer_ms, Some(reply));
        assert_eq!(state.last_real_answer_ms, Some(1_000));
        assert_eq!(state.last_ping_ms, Some(2_000));
        assert_eq!(state.pings_sent, 1);
        assert!(!state.ping_outstanding);

        // A reply observed after the window is still the reply to the outstanding ping.
        record_ping(&mut state, reply + 10);
        let outside_the_window = reply + 10 + PING_REPLY_WINDOW_MS + 1;
        observe_answer(&mut state, Some(outside_the_window), outside_the_window);
        assert_eq!(state.last_seen_answer_ms, Some(outside_the_window));
        assert_eq!(state.last_real_answer_ms, Some(1_000));
        assert_eq!(state.last_ping_ms, Some(reply + 10));
        assert_eq!(state.pings_sent, 2);
        assert!(!state.ping_outstanding);

        let frozen = state.clone();
        observe_answer(&mut state, None, outside_the_window);
        observe_answer(&mut state, Some(outside_the_window), outside_the_window);
        assert_eq!(state, frozen);
    }

    /// One reply is written as several usage records seconds apart, and the scan can see them one at a
    /// time. Every one of them inside the window is the reply; none may restart the idle clock.
    #[test]
    fn a_reply_seen_across_two_scans_does_not_restart_the_idle_clock() {
        let mut state = KeepAliveState {
            enabled: true,
            last_real_answer_ms: Some(1_000),
            last_seen_answer_ms: Some(1_000),
            last_ping_ms: Some(5_000),
            pings_sent: 3,
            ping_outstanding: true,
        };
        observe_answer(&mut state, Some(6_000), 6_500);
        observe_answer(&mut state, Some(7_800), 7_900);
        assert_eq!(
            (
                state.last_real_answer_ms,
                state.pings_sent,
                state.last_seen_answer_ms
            ),
            (Some(1_000), 3, Some(7_800)),
            "both records of the reply must leave the idle clock and ping count alone"
        );
        assert!(!state.ping_outstanding);

        let after_the_window = 5_000 + PING_REPLY_WINDOW_MS + 1;
        observe_answer(&mut state, Some(after_the_window), after_the_window);
        assert_eq!(
            (state.last_real_answer_ms, state.pings_sent),
            (Some(after_the_window), 0),
            "an answer after the window, with the ping already answered, is a real answer"
        );
    }

    #[test]
    fn observe_answer_recognises_a_reply_timestamped_earlier_than_the_ping() {
        let mut state = KeepAliveState {
            enabled: true,
            last_real_answer_ms: Some(1_000),
            last_seen_answer_ms: Some(1_000),
            last_ping_ms: Some(5_000),
            pings_sent: 2,
            ping_outstanding: true,
        };
        observe_answer(&mut state, Some(2_000), 5_000);
        assert_eq!(state.last_seen_answer_ms, Some(2_000));
        assert_eq!(state.last_real_answer_ms, Some(1_000));
        assert_eq!(state.last_ping_ms, Some(5_000));
        assert_eq!(state.pings_sent, 2);
        assert!(!state.ping_outstanding);
    }

    #[test]
    fn turning_keep_alive_on_starts_the_idle_clock_at_the_newest_answer() {
        let mut state = KeepAliveState {
            enabled: false,
            last_real_answer_ms: None,
            last_seen_answer_ms: Some(40),
            last_ping_ms: Some(9),
            pings_sent: 3,
            ping_outstanding: true,
        };
        set_enabled(&mut state, true);
        assert!(state.enabled);
        assert_eq!(state.last_real_answer_ms, Some(40));
        assert_eq!(state.last_ping_ms, None);
        assert_eq!(state.pings_sent, 0);
        assert!(!state.ping_outstanding);

        // Turning on replaces an idle clock that was already running.
        state.last_real_answer_ms = Some(7);
        state.last_seen_answer_ms = Some(40);
        state.last_ping_ms = Some(9);
        state.pings_sent = 3;
        state.ping_outstanding = true;
        set_enabled(&mut state, true);
        assert_eq!(state.last_real_answer_ms, Some(40));
        assert_eq!(state.last_ping_ms, None);
        assert_eq!(state.pings_sent, 0);
        assert!(!state.ping_outstanding);

        state.last_ping_ms = Some(11);
        state.pings_sent = 2;
        state.ping_outstanding = true;
        set_enabled(&mut state, false);
        assert!(!state.enabled);
        assert_eq!(state.last_real_answer_ms, Some(40));
        assert_eq!(state.last_seen_answer_ms, Some(40));
        assert_eq!(state.last_ping_ms, Some(11));
        assert_eq!(state.pings_sent, 2);
        assert!(state.ping_outstanding);

        state.last_seen_answer_ms = None;
        set_enabled(&mut state, true);
        assert_eq!(state.last_real_answer_ms, Some(40));
    }

    #[test]
    fn keep_alive_config_clamps_out_of_range_values_and_rejects_a_blank_message() {
        let defaults = KeepAliveConfig::new(None, None, None);
        assert_eq!(defaults.interval_ms, 50 * MILLISECONDS_PER_MINUTE);
        assert_eq!(defaults.max_idle_ms, 12 * MILLISECONDS_PER_HOUR);
        assert_eq!(defaults.message, DEFAULT_MESSAGE);
        assert_eq!(defaults, KeepAliveConfig::default());

        let low = KeepAliveConfig::new(Some(1), Some(0), Some("   ".to_string()));
        assert_eq!(low.interval_ms, 5 * MILLISECONDS_PER_MINUTE);
        assert_eq!(low.max_idle_ms, MILLISECONDS_PER_HOUR);
        assert_eq!(low.message, DEFAULT_MESSAGE);

        let high = KeepAliveConfig::new(Some(61), Some(49), Some("  ping  ".to_string()));
        assert_eq!(high.interval_ms, 55 * MILLISECONDS_PER_MINUTE);
        assert_eq!(high.max_idle_ms, 48 * MILLISECONDS_PER_HOUR);
        assert_eq!(high.message, "  ping  ");

        let inside = KeepAliveConfig::new(Some(5), Some(48), Some("ok".to_string()));
        assert_eq!(inside.interval_ms, 5 * MILLISECONDS_PER_MINUTE);
        assert_eq!(inside.max_idle_ms, 48 * MILLISECONDS_PER_HOUR);
        assert_eq!(inside.message, "ok");
    }

    #[test]
    fn format_countdown_buckets_minutes_and_hours() {
        assert_eq!(format_countdown(-1), "0m");
        assert_eq!(format_countdown(0), "<1m");
        assert_eq!(format_countdown(59_999), "<1m");
        assert_eq!(format_countdown(60_000), "1m");
        assert_eq!(format_countdown(59 * 60_000), "59m");
        assert_eq!(format_countdown(60 * 60_000), "1h00m");
        assert_eq!(format_countdown(65 * 60_000), "1h05m");
        assert_eq!(format_countdown(90 * 60_000 + 30_000), "1h30m");
    }

    #[test]
    fn ping_and_rewrite_cost_prices_a_read_against_an_hour_long_write() {
        let rates = ModelRates {
            input: 5.,
            output: 25.,
            cache_write_1h: 10.,
            cache_write_5m: 6.25,
            cache_read: 0.5,
        };
        assert_eq!(ping_and_rewrite_cost(2_000_000, rates), (1., 20.));
    }
}
