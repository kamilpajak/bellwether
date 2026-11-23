//! The cost cap: three limits over the whole store, checked before every call. Money is integer
//! micro-USD. `docs/design/architecture.md` section 3.5, row `cost`.

const MICROS_PER_USD: i64 = 1_000_000;
const FRACTION_DIGITS: usize = 6;
const BYTES_PER_MTOK: i64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a USD amount: digits, optionally `.` and at most {FRACTION_DIGITS} more")]
pub struct AmountError(pub String);

/// A decimal USD amount as micro-USD, with no float in between.
pub fn parse_usd(text: &str) -> Result<i64, AmountError> {
    let refused = || AmountError(text.to_owned());
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) || fraction.len() > FRACTION_DIGITS {
        return Err(refused());
    }
    if text.contains('.') && fraction.is_empty() {
        return Err(refused());
    }
    let padded = format!("{fraction:0<FRACTION_DIGITS$}");
    let whole: i64 = whole.parse().map_err(|_| refused())?;
    let fraction: i64 = padded.parse().map_err(|_| refused())?;
    whole
        .checked_mul(MICROS_PER_USD)
        .and_then(|micros| micros.checked_add(fraction))
        .ok_or_else(refused)
}

/// What a call can cost at most IF one input token is at least one byte and each byte is billed
/// once: bytes x price, rounded up. That premise is the vendor's to break, which is what
/// `cost_estimate_broken` latches on (section 3.5).
#[must_use]
pub fn worst_case(request_bytes: usize, price_micro_usd_per_mtok: i64) -> i64 {
    let bytes = i64::try_from(request_bytes).unwrap_or(i64::MAX);
    bytes
        .saturating_mul(price_micro_usd_per_mtok)
        .saturating_add(BYTES_PER_MTOK - 1)
        / BYTES_PER_MTOK
}

/// A vendor cost in USD as micro-USD, rounded up. `None` for a value that is not a cost.
#[must_use]
pub fn micros_of(cost_usd: f64) -> Option<i64> {
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return None;
    }
    let micros = (cost_usd * 1e6).ceil();
    #[allow(clippy::cast_possible_truncation)]
    (micros < 9e18).then_some(micros as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub run: i64,
    pub day: i64,
    pub total: i64,
}

/// Micro-USD already spent in each scope. Unknown spend counts as spent: an open, an abandoned
/// and a cost-less closed attempt each count at their worst case (`Store::spent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Spent {
    pub run: i64,
    pub day: i64,
    pub total: i64,
}

impl Spent {
    pub fn add(&mut self, micros: i64) {
        self.run = self.run.saturating_add(micros);
        self.day = self.day.saturating_add(micros);
        self.total = self.total.saturating_add(micros);
    }
}

/// The scope a call of `next` micro-USD at worst would overrun, checked narrowest first.
#[must_use]
pub fn refusal(caps: Caps, spent: Spent, next: i64) -> Option<&'static str> {
    let over = |spent: i64, cap: i64| spent.saturating_add(next) > cap;
    if over(spent.run, caps.run) {
        Some("run")
    } else if over(spent.day, caps.day) {
        Some("day")
    } else if over(spent.total, caps.total) {
        Some("total")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{micros_of, parse_usd, refusal, worst_case, Caps, Spent};

    #[test]
    fn a_usd_amount_parses_to_micro_usd_without_a_float() {
        let parsed = ["0.042", "1.50", "40", "0.000001", "0"].map(parse_usd);
        assert_eq!(
            parsed,
            [Ok(42_000), Ok(1_500_000), Ok(40_000_000), Ok(1), Ok(0)]
        );
        for refused in ["", ".5", "1.", "-1", "1e3", "0.0000001", "1,5", " 1"] {
            assert!(parse_usd(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn the_worst_case_is_bytes_times_price_rounded_up() {
        assert_eq!(worst_case(30_000, 42_000), 1_260);
        assert_eq!(worst_case(1, 42_000), 1);
        assert_eq!(worst_case(0, 42_000), 0);
    }

    #[test]
    fn a_vendor_cost_rounds_up_and_a_non_cost_is_none() {
        assert_eq!(micros_of(0.000_120_1), Some(121));
        assert_eq!(micros_of(0.0), Some(0));
        assert_eq!(micros_of(-0.1), None);
        assert_eq!(micros_of(f64::NAN), None);
    }

    /// Section 8 row MG3. Mutation 2026-11-23, `>` inverted to `<`: red.
    #[test]
    fn a_call_is_refused_by_the_first_scope_it_would_overrun_and_allowed_at_the_cap_exactly() {
        let caps = Caps {
            run: 100,
            day: 200,
            total: 300,
        };
        let spent = |run, day, total| Spent { run, day, total };
        assert_eq!(refusal(caps, spent(90, 90, 90), 10), None);
        assert_eq!(refusal(caps, spent(91, 91, 91), 10), Some("run"));
        assert_eq!(refusal(caps, spent(0, 191, 191), 10), Some("day"));
        assert_eq!(refusal(caps, spent(0, 0, 291), 10), Some("total"));
    }
}
