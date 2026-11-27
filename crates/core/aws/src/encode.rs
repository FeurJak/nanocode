const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const HEX_DIGITS_UPPER: &[u8; 16] = b"0123456789ABCDEF";
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3_600;
const SECONDS_PER_MINUTE: u64 = 60;
const YEAR_MAX: u64 = 9_999;

pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    assert_eq!(text.len(), bytes.len() * 2);
    text
}

pub fn uri_encode(out: &mut String, text: &str, keep_slash: bool) {
    for &byte in text.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (keep_slash && byte == b'/') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX_DIGITS_UPPER[usize::from(byte >> 4)]));
            out.push(char::from(HEX_DIGITS_UPPER[usize::from(byte & 0x0f)]));
        }
    }
}

pub struct UtcTime {
    pub amz_date: String,
    pub date: String,
}

pub fn utc(unix_seconds: u64) -> UtcTime {
    let days = unix_seconds / SECONDS_PER_DAY;
    let seconds_of_day = unix_seconds % SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days);
    assert!(year <= YEAR_MAX);
    assert!((1..=12).contains(&month));
    assert!((1..=31).contains(&day));
    let hour = seconds_of_day / SECONDS_PER_HOUR;
    let minute = seconds_of_day % SECONDS_PER_HOUR / SECONDS_PER_MINUTE;
    let second = seconds_of_day % SECONDS_PER_MINUTE;
    let date = format!("{year:04}{month:02}{day:02}");
    let amz_date = format!("{date}T{hour:02}{minute:02}{second:02}Z");
    assert_eq!(amz_date.len(), 16);
    UtcTime { amz_date, date }
}

const fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
