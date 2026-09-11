//! What a session has spent, read from the usage Claude Code records beside each of its
//! answers.
//!
//! The transcript carries no price of any kind — only token counts — so the cost here is
//! those counts against the published per-model rates. A model whose rates are not known
//! yields no cost at all rather than a wrong one: the tokens are still reported, and the
//! caller shows those alone.

use serde_json::Value;

/// Tokens one answer was billed for.
///
/// The cache fields are kept apart because they are priced apart: writing an entry that
/// lives an hour costs twice the base input rate, writing a five-minute one costs a
/// quarter more, and reading either costs a tenth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    /// Part of `output_tokens`, not additional to it. Reported because it says how much
    /// of an answer was the model thinking rather than writing.
    pub thinking_tokens: u64,
}

impl Usage {
    /// Reads the `usage` object Claude Code writes inside an assistant record's message.
    ///
    /// `None` when the record has no usage at all, which is every record that is not an
    /// answer from the model.
    pub fn from_record(raw: &Value) -> Option<Self> {
        let usage = raw.get("message")?.get("usage")?;
        let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);

        // `cache_creation` splits the write by how long the entry lives; the flat
        // `cache_creation_input_tokens` beside it is their total, and is the fallback for
        // a record written before the split existed.
        let creation = usage.get("cache_creation");
        let write_1h = number(creation.and_then(|c| c.get("ephemeral_1h_input_tokens")));
        let write_5m = number(creation.and_then(|c| c.get("ephemeral_5m_input_tokens")));
        let write_total = number(usage.get("cache_creation_input_tokens"));
        let (write_1h, write_5m) = if write_1h == 0 && write_5m == 0 {
            // Unsplit, so it is attributed to the five-minute TTL: that is the default
            // one, and pricing it as the hour would overstate the cost.
            (0, write_total)
        } else {
            (write_1h, write_5m)
        };

        Some(Self {
            input_tokens: number(usage.get("input_tokens")),
            cache_write_1h_tokens: write_1h,
            cache_write_5m_tokens: write_5m,
            cache_read_tokens: number(usage.get("cache_read_input_tokens")),
            output_tokens: number(usage.get("output_tokens")),
            thinking_tokens: number(
                usage
                    .get("output_tokens_details")
                    .and_then(|details| details.get("thinking_tokens")),
            ),
        })
    }

    /// Everything the model was given for this answer: what was sent fresh, plus what was
    /// read from the cache, plus what was written into it. This is the size of the
    /// context that produced the answer.
    pub fn context_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_1h_tokens)
            .saturating_add(self.cache_write_5m_tokens)
    }

    pub fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cache_write_1h_tokens: self
                .cache_write_1h_tokens
                .saturating_add(other.cache_write_1h_tokens),
            cache_write_5m_tokens: self
                .cache_write_5m_tokens
                .saturating_add(other.cache_write_5m_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            thinking_tokens: self.thinking_tokens.saturating_add(other.thinking_tokens),
        }
    }

    /// What these tokens cost in US dollars at `rates`.
    pub fn cost(self, rates: ModelRates) -> f64 {
        const PER_MILLION: f64 = 1_000_000.;
        let at = |tokens: u64, rate: f64| tokens as f64 * rate / PER_MILLION;

        at(self.input_tokens, rates.input)
            + at(self.cache_write_1h_tokens, rates.cache_write_1h)
            + at(self.cache_write_5m_tokens, rates.cache_write_5m)
            + at(self.cache_read_tokens, rates.cache_read)
            + at(self.output_tokens, rates.output)
    }
}

/// US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelRates {
    pub input: f64,
    pub output: f64,
    pub cache_write_1h: f64,
    pub cache_write_5m: f64,
    pub cache_read: f64,
}

impl ModelRates {
    /// Derives the cache rates from the input rate, which is how they are published: an
    /// hour-long write costs twice the input rate, a five-minute write a quarter more,
    /// and a read a tenth.
    const fn from_input_and_output(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_write_1h: input * 2.,
            cache_write_5m: input * 1.25,
            cache_read: input * 0.1,
        }
    }
}

/// The rates for the model named in an assistant record, or `None` for a model this does
/// not know.
///
/// Matched on the family rather than the exact id so that a dated snapshot of a known
/// model is still priced. An unknown model is left unpriced on purpose — a session's cost
/// is worth nothing if it might be the wrong number.
pub fn rates_for_model(model: &str) -> Option<ModelRates> {
    // Ordered longest-prefix first, so that `claude-opus-4-8` is not read as an Opus 4.
    const RATES: &[(&str, f64, f64)] = &[
        ("claude-fable-5", 10., 50.),
        ("claude-mythos-5", 10., 50.),
        ("claude-opus-5", 5., 25.),
        ("claude-opus-4-8", 5., 25.),
        ("claude-opus-4-7", 5., 25.),
        ("claude-opus-4-6", 5., 25.),
        ("claude-sonnet-5", 2., 10.),
        ("claude-sonnet-4-6", 3., 15.),
        ("claude-haiku-4-5", 1., 5.),
    ];

    RATES
        .iter()
        .find(|(family, _, _)| model.starts_with(family))
        .map(|(_, input, output)| ModelRates::from_input_and_output(*input, *output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_of(json: &str) -> Usage {
        let raw: Value = serde_json::from_str(json).expect("test json parses");
        Usage::from_record(&raw).expect("the record carries usage")
    }

    #[test]
    fn the_split_cache_write_is_read_by_its_ttl() {
        let usage = usage_of(
            r#"{"message":{"usage":{"input_tokens":2,"output_tokens":488,
"cache_read_input_tokens":29592,"cache_creation_input_tokens":27456,
"cache_creation":{"ephemeral_1h_input_tokens":27456,"ephemeral_5m_input_tokens":0},
"output_tokens_details":{"thinking_tokens":335}}}}"#,
        );

        assert_eq!(
            usage,
            Usage {
                input_tokens: 2,
                cache_write_1h_tokens: 27456,
                cache_write_5m_tokens: 0,
                cache_read_tokens: 29592,
                output_tokens: 488,
                thinking_tokens: 335,
            }
        );
    }

    /// A write with no split is priced as the cheaper of the two, because the
    /// five-minute TTL is the default one and guessing the hour would overstate it.
    #[test]
    fn an_unsplit_cache_write_is_priced_as_the_default_ttl() {
        let usage = usage_of(
            r#"{"message":{"usage":{"input_tokens":10,"output_tokens":20,
"cache_creation_input_tokens":1000}}}"#,
        );

        assert_eq!(usage.cache_write_5m_tokens, 1000);
        assert_eq!(usage.cache_write_1h_tokens, 0);
    }

    #[test]
    fn a_record_with_no_usage_has_none() {
        let raw: Value = serde_json::from_str(r#"{"type":"user","message":{"content":"hi"}}"#)
            .expect("test json parses");
        assert_eq!(Usage::from_record(&raw), None);
    }

    #[test]
    fn the_context_is_everything_the_model_was_given() {
        let usage = Usage {
            input_tokens: 2,
            cache_write_1h_tokens: 27_456,
            cache_write_5m_tokens: 0,
            cache_read_tokens: 29_592,
            output_tokens: 488,
            thinking_tokens: 335,
        };

        assert_eq!(
            usage.context_tokens(),
            57_050,
            "the answer's own output is not part of what it was given"
        );
    }

    #[test]
    fn cost_is_each_kind_of_token_at_its_own_rate() {
        let rates = rates_for_model("claude-opus-5").expect("opus 5 is priced");
        assert_eq!(rates.input, 5.);
        assert_eq!(rates.output, 25.);
        assert_eq!(rates.cache_write_1h, 10.);
        assert_eq!(rates.cache_write_5m, 6.25);
        assert_eq!(rates.cache_read, 0.5);

        let one_million_of_each = Usage {
            input_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            cache_write_5m_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            output_tokens: 1_000_000,
            thinking_tokens: 0,
        };

        assert_eq!(
            one_million_of_each.cost(rates),
            5. + 10. + 6.25 + 0.5 + 25.,
            "thinking tokens are part of the output and must not be billed twice"
        );
    }

    #[test]
    fn a_dated_snapshot_is_priced_as_its_family() {
        assert_eq!(
            rates_for_model("claude-opus-5-20260401"),
            rates_for_model("claude-opus-5")
        );
    }

    /// The families whose names would collide if the table were matched shortest-first.
    #[test]
    fn each_family_is_priced_as_itself() {
        assert_eq!(
            rates_for_model("claude-opus-4-8").map(|r| r.input),
            Some(5.)
        );
        assert_eq!(
            rates_for_model("claude-sonnet-5").map(|r| r.input),
            Some(2.)
        );
        assert_eq!(
            rates_for_model("claude-sonnet-4-6").map(|r| r.input),
            Some(3.)
        );
        assert_eq!(
            rates_for_model("claude-haiku-4-5").map(|r| r.input),
            Some(1.)
        );
        assert_eq!(
            rates_for_model("claude-fable-5-1").map(|r| r.input),
            Some(10.)
        );
    }

    /// An unknown model must yield no price rather than a wrong one.
    #[test]
    fn an_unknown_model_is_not_priced() {
        assert_eq!(rates_for_model("some-other-model"), None);
        assert_eq!(rates_for_model("gpt-4"), None);
    }
}
