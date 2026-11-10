//! A five-field cron expression in words, for the common shapes.

/// `0 9 * * MON-FRI` as `MON-FRI 09:00`, `*/15 * * * *` as `every 15 min`. `None` for anything
/// that names a day of the month, a month, or a shape not listed here.
pub fn words(expr: &str) -> Option<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let [minute, hour, "*", "*", weekday] = fields.as_slice() else {
        return None;
    };
    let days = match *weekday {
        "*" => "daily".to_string(),
        days => days.to_string(),
    };
    if let Some(every) = step(minute) {
        return (*hour == "*" && *weekday == "*").then(|| format!("every {every} min"));
    }
    let minute: u8 = minute.parse().ok().filter(|minute| *minute < 60)?;
    if *hour == "*" {
        return Some(format!("{} at :{minute:02}", hourly(&days, 1)));
    }
    if let Some(every) = step(hour) {
        return Some(format!("{} at :{minute:02}", hourly(&days, every)));
    }
    let times = hour
        .split(',')
        .map(|hour| {
            let hour: u8 = hour.parse().ok().filter(|hour| *hour < 24)?;
            Some(format!("{hour:02}:{minute:02}"))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(format!("{days} {}", times.join(", ")))
}

/// The `N` of `*/N`.
fn step(field: &str) -> Option<u8> {
    field
        .strip_prefix("*/")?
        .parse()
        .ok()
        .filter(|every| *every > 0)
}

fn hourly(days: &str, every: u8) -> String {
    let rate = if every == 1 {
        "hourly".to_string()
    } else {
        format!("every {every} h")
    };
    if days == "daily" {
        rate
    } else {
        format!("{days} {rate}")
    }
}

#[cfg(test)]
mod tests {
    use crate::cron::words;

    #[test]
    fn common_expressions_read_as_words() {
        let read = |expr| words(expr).unwrap();
        assert_eq!(read("0 9 * * *"), "daily 09:00");
        assert_eq!(read("30 7 * * MON-FRI"), "MON-FRI 07:30");
        assert_eq!(
            read("0 1,7,13,19 * * *"),
            "daily 01:00, 07:00, 13:00, 19:00"
        );
        assert_eq!(read("0 * * * *"), "hourly at :00");
        assert_eq!(read("15 */6 * * *"), "every 6 h at :15");
        assert_eq!(read("5 */2 * * 1-5"), "1-5 every 2 h at :05");
        assert_eq!(read("*/15 * * * *"), "every 15 min");
        assert_eq!(read("  0   9 * * 0 "), "0 09:00");
    }

    #[test]
    fn anything_else_is_left_as_written() {
        for expr in [
            "",
            "0 9 * *",
            "0 9 1 * *",
            "0 9 * 6 *",
            "0 9-17 * * *",
            "61 9 * * *",
            "0 24 * * *",
            "*/0 * * * *",
            "*/15 3 * * *",
            "*/15 * * * MON",
            "@daily",
            "0 0 9 * * *",
        ] {
            assert_eq!(words(expr), None, "{expr}");
        }
    }
}
