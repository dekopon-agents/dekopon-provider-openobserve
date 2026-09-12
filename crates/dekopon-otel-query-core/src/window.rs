//! `--since`, parsed once and capped once.
//!
//! Every word takes a window and the provider bounds it itself, because `ExecutionConstraints` is
//! `deny_unknown_fields` and a query-window key would be tree growth in dekopon for a bound only
//! this provider needs. The cap is hard-coded at 30 days and stated in `--help`: the homelab store
//! retains 30 days, so a longer window returns nothing anyway, and a bound a model can read is
//! worth more than one it discovers by refusal.

use std::fmt;

/// The largest window any word will accept, in seconds.
pub const MAX_WINDOW_SECONDS: u64 = 30 * 24 * 60 * 60;

/// The smallest window worth a request.
pub const MIN_WINDOW_SECONDS: u64 = 1;

/// Why a `--since` value was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowError {
    /// The value did not parse as `<integer><unit>`.
    Malformed(String),
    /// The value parsed but exceeded the 30-day cap.
    TooLong { seconds: u64 },
    /// The value parsed to zero.
    TooShort,
}

impl fmt::Display for WindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(value) => write!(
                formatter,
                "--since {value}: expected a duration like 30m, 24h, 7d (units s, m, h, d)"
            ),
            Self::TooLong { seconds } => write!(
                formatter,
                "--since is {seconds}s; the maximum window is {MAX_WINDOW_SECONDS}s (30d)"
            ),
            Self::TooShort => formatter.write_str("--since must be at least 1s"),
        }
    }
}

/// A closed time window, resolved against the clock the caller supplied.
///
/// Microseconds, because that is the unit OpenObserve's `_search` takes for `start_time` and
/// `end_time` and the unit a Jaeger-compatible backend takes too. Storing them resolved rather than
/// as "24h ago" is what makes a plan deterministic under test: the fixtures assert exact integers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
    start_us: u64,
    end_us: u64,
}

impl Window {
    /// Builds a window ending at `now_us` and reaching `seconds` back.
    #[must_use]
    pub fn ending_at(now_us: u64, seconds: u64) -> Self {
        Self {
            start_us: now_us.saturating_sub(seconds.saturating_mul(1_000_000)),
            end_us: now_us,
        }
    }

    /// Inclusive start, microseconds since the Unix epoch.
    #[must_use]
    pub fn start_us(&self) -> u64 {
        self.start_us
    }

    /// Exclusive end, microseconds since the Unix epoch.
    #[must_use]
    pub fn end_us(&self) -> u64 {
        self.end_us
    }

    /// The window as the `{"since":…,"until":…}` object every output shape carries.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sinceUs": self.start_us,
            "untilUs": self.end_us,
        })
    }
}

/// Parses `<integer><unit>` into seconds and applies the 30-day cap.
///
/// Deliberately not a general duration parser: no fractions, no compound `1h30m`, no unit beyond
/// `s`/`m`/`h`/`d`. A model that types something else gets a usage error naming the four units,
/// which is a shorter road to a working call than a parser that quietly accepts `1.5h`.
pub fn parse_since(value: &str) -> Result<u64, WindowError> {
    let malformed = || WindowError::Malformed(value.to_owned());
    let (digits, unit) = value.split_at(value.len().saturating_sub(1));
    let multiplier = match unit {
        "s" => 1_u64,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(malformed()),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    let seconds = digits
        .parse::<u64>()
        .map_err(|_| malformed())?
        .checked_mul(multiplier)
        .ok_or_else(malformed)?;
    if seconds < MIN_WINDOW_SECONDS {
        return Err(WindowError::TooShort);
    }
    if seconds > MAX_WINDOW_SECONDS {
        return Err(WindowError::TooLong { seconds });
    }
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::{MAX_WINDOW_SECONDS, Window, WindowError, parse_since};

    #[test]
    fn the_four_units_parse_and_nothing_else_does() {
        assert_eq!(parse_since("90s"), Ok(90));
        assert_eq!(parse_since("30m"), Ok(1_800));
        assert_eq!(parse_since("24h"), Ok(86_400));
        assert_eq!(parse_since("7d"), Ok(604_800));
        for bad in ["", "h", "24", "1.5h", "24H", "1h30m", "-1h", "24w", " 24h"] {
            assert!(
                matches!(parse_since(bad), Err(WindowError::Malformed(_))),
                "{bad} was accepted"
            );
        }
    }

    /// The cap is the provider's own, so it is asserted rather than assumed: 30d passes, 31d and
    /// 721h are refused with the number in the message.
    #[test]
    fn thirty_days_is_the_ceiling_and_zero_is_the_floor() {
        assert_eq!(parse_since("30d"), Ok(MAX_WINDOW_SECONDS));
        assert_eq!(parse_since("720h"), Ok(MAX_WINDOW_SECONDS));
        assert_eq!(
            parse_since("31d"),
            Err(WindowError::TooLong {
                seconds: 31 * 24 * 60 * 60
            })
        );
        assert_eq!(
            parse_since("721h"),
            Err(WindowError::TooLong {
                seconds: 721 * 60 * 60
            })
        );
        assert_eq!(parse_since("0s"), Err(WindowError::TooShort));
        assert!(
            parse_since("31d").unwrap_err().to_string().contains("30d"),
            "the refusal names the cap"
        );
    }

    /// Microsecond arithmetic, exactly: this is what ends up in the request body.
    #[test]
    fn the_window_resolves_to_microseconds() {
        let window = Window::ending_at(1_789_000_000_000_000, 86_400);
        assert_eq!(window.end_us(), 1_789_000_000_000_000);
        assert_eq!(window.start_us(), 1_788_913_600_000_000);
        assert_eq!(window.end_us() - window.start_us(), 86_400_000_000);
    }

    /// A clock that has not been set yet must not underflow into the far future.
    #[test]
    fn a_window_longer_than_the_clock_saturates_at_the_epoch() {
        assert_eq!(Window::ending_at(1_000, 86_400).start_us(), 0);
    }
}
