//! Host-local RTC without a date/time crate.
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn datetime_bcd() -> [u8; 7] {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let d = local_date(seconds);
    d.map(|v| (v / 10) * 16 + v % 10)
}

// Gregorian civil date from days since 1970-01-01 (400-year eras).
fn civil_parts(seconds: i64) -> [i64; 7] {
    let days = seconds.div_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let t = seconds.rem_euclid(86400);
    [
        year,
        month,
        day,
        (days + 4).rem_euclid(7),
        t / 3600,
        t / 60 % 60,
        t % 60,
    ]
}
fn civil(seconds: i64) -> [u8; 7] {
    let mut d = civil_parts(seconds);
    d[0] = d[0].rem_euclid(100);
    d.map(|v| v as u8)
}
#[cfg(windows)]
fn local_date(seconds: i64) -> [u8; 7] {
    #[repr(C)]
    #[derive(Default)]
    struct Date {
        year: u16,
        month: u16,
        weekday: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millis: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SystemTimeToTzSpecificLocalTime(
            zone: *const std::ffi::c_void,
            utc: *const Date,
            local: *mut Date,
        ) -> i32;
    }
    let d = civil(seconds);
    let utc = Date {
        year: civil_parts(seconds)[0] as u16,
        month: d[1] as u16,
        weekday: d[3] as u16,
        day: d[2] as u16,
        hour: d[4] as u16,
        minute: d[5] as u16,
        second: d[6] as u16,
        millis: 0,
    };
    let mut local = Date::default();
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return d;
    }
    [
        (local.year % 100) as u8,
        local.month as u8,
        local.day as u8,
        local.weekday as u8,
        local.hour as u8,
        local.minute as u8,
        local.second as u8,
    ]
}

#[cfg(unix)]
fn local_date(seconds: i64) -> [u8; 7] {
    use std::ffi::{c_char, c_int, c_long};
    #[repr(C)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        day: c_int,
        mon: c_int,
        year: c_int,
        weekday: c_int,
        yearday: c_int,
        dst: c_int,
        offset: c_long,
        zone: *const c_char,
    }
    unsafe extern "C" {
        fn localtime_r(time: *const c_long, result: *mut Tm) -> *mut Tm;
    }
    let time = seconds as c_long;
    let mut result = std::mem::MaybeUninit::<Tm>::uninit();
    if unsafe { localtime_r(&time, result.as_mut_ptr()) }.is_null() {
        return civil(seconds);
    }
    let d = unsafe { result.assume_init() };
    [
        (d.year + 1900).rem_euclid(100) as u8,
        (d.mon + 1) as u8,
        d.day as u8,
        d.weekday as u8,
        d.hour as u8,
        d.min as u8,
        d.sec as u8,
    ]
}

#[cfg(not(any(windows, unix)))]
fn local_date(seconds: i64) -> [u8; 7] {
    civil(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn civil_boundaries() {
        assert_eq!(civil(0), [70, 1, 1, 4, 0, 0, 0]);
        assert_eq!(civil(951782400), [0, 2, 29, 2, 0, 0, 0]);
        assert_eq!(civil(1709251199), [24, 2, 29, 4, 23, 59, 59]);
        assert_eq!(civil(1709251200), [24, 3, 1, 5, 0, 0, 0]);
    }
}
