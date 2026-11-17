//! How a view's numbers are written: on axes, in tooltips, on bars and beside ranges.

/// `text`, a fixed-point number, without the zeros that end its fraction.
fn trimmed(text: String) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// A tick's label: `value` with as many decimals as `step` needs, thousands as k, M, B or T.
pub(crate) fn number(value: f64, step: f64) -> String {
    if value == 0.0 || !value.is_finite() {
        return "0".to_string();
    }
    let (unit, suffix) = [(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "k")]
        .into_iter()
        .find(|(unit, _)| value.abs() >= *unit)
        .unwrap_or((1.0, ""));
    let decimals = -(step.abs() / unit).log10().floor();
    let decimals = if decimals.is_finite() {
        decimals.max(0.0) as usize
    } else {
        0
    };
    if decimals > 4 && value.abs() < 1e-3 {
        let digits = (value.abs().log10().floor() - step.abs().log10().floor()).clamp(0.0, 17.0);
        return scientific(value, digits as usize);
    }
    format!(
        "{}{suffix}",
        trimmed(format!("{:.*}", decimals, value / unit))
    )
}

/// `value` to four significant figures, written as a tick is.
pub(crate) fn figure(value: f64) -> String {
    if value == 0.0 || !value.is_finite() {
        return "0".to_string();
    }
    let step = 10_f64.powf(value.abs().log10().floor() - 3.0);
    if step > 0.0 && step.is_finite() {
        number(value, step)
    } else {
        scientific(value, 3)
    }
}

fn scientific(value: f64, digits: usize) -> String {
    let text = format!("{:.*e}", digits, value);
    match text.split_once('e') {
        Some((mantissa, exponent)) => format!("{}e{exponent}", trimmed(mantissa.to_string())),
        None => text,
    }
}

/// `m` times ten to the `e`.
pub(crate) fn power(m: i32, e: i32) -> String {
    if (-4..=15).contains(&e) {
        let value = f64::from(m) * 10_f64.powi(e);
        number(value, value)
    } else {
        format!("{m}e{e}")
    }
}

/// `value` whole when it is, and to one decimal otherwise.
pub(crate) fn short(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// `value` with as many decimals as `step` has, up to six.
pub(crate) fn stepped(value: f64, step: f64) -> String {
    let mut places = 0;
    let mut scaled = step;
    while places < 6 && (scaled - scaled.round()).abs() > 1e-9 * scaled.abs() {
        scaled *= 10.0;
        places += 1;
    }
    format!("{value:.places$}")
}

#[cfg(test)]
mod tests {
    use crucible_view::MAGNITUDE;

    use crate::ui::report::figures::{figure, number, short, stepped};

    #[test]
    fn numbers_are_written_compactly() {
        assert_eq!(number(1234.0, 100.0), "1.2k");
        assert_eq!(number(3_400_000.0, 100_000.0), "3.4M");
        assert_eq!(number(2e9, 1e9), "2B");
        assert_eq!(number(-1500.0, 500.0), "-1.5k");
        assert_eq!(number(500.0, 500.0), "500");
        assert_eq!(number(0.25, 0.05), "0.25");
        assert_eq!(number(0.0, 0.05), "0");
        assert_eq!(number(f64::NAN, 1.0), "0");
        assert_eq!(figure(57.0), "57");
        assert_eq!(figure(0.1 + 0.2), "0.3");
        assert_eq!(figure(1_234_567.0), "1.235M");
        assert_eq!(figure(-0.000_012_34), "-1.234e-5");
        assert_eq!(figure(5e-324), "4.941e-324");
        assert_eq!(figure(MAGNITUDE), "1000T");
        assert_eq!(figure(7.5), "7.5");
    }

    #[test]
    fn a_short_number_is_whole_or_has_one_decimal() {
        assert_eq!(short(34.0), "34");
        assert_eq!(short(2.5), "2.5");
        assert_eq!(short(0.04), "0.0");
        assert_eq!(short(1_234_567.0), "1234567");
    }

    #[test]
    fn a_range_s_value_shows_the_decimals_its_step_has() {
        assert_eq!(stepped(7.0, 1.0), "7");
        assert_eq!(stepped(7.5, 0.5), "7.5");
        assert_eq!(stepped(0.3, 0.1), "0.3");
        assert_eq!(stepped(0.25, 0.05), "0.25");
        assert_eq!(stepped(1e15, 1e14), "1000000000000000");
        assert_eq!(stepped(-2.0, 2.0), "-2");
        assert_eq!(stepped(1.0, 1e-12), "1.000000");
        assert_eq!(stepped(3.0, f64::MIN_POSITIVE), "3.000000");
    }
}
