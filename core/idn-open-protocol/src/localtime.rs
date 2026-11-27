//! The machine's local time (a clock on a title, a recording's folder name): the operating
//! system's own conversion (`localtime_r`, `GetLocalTime`), so the time zone and summer
//! time are the machine's. (Rust's standard library has no local time; the `time` crate
//! refuses it once a process has threads.)

/// A moment in local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// The year, e.g. 2026.
    pub year: i32,
    /// 1–12.
    pub month: u8,
    /// 1–31.
    pub day: u8,
    /// 0–23.
    pub hour: u8,
    /// 0–59.
    pub minute: u8,
    /// 0–59.
    pub second: u8,
    /// 0 Sunday – 6 Saturday.
    pub weekday: u8,
}

impl LocalTime {
    /// Seconds since midnight.
    pub fn of_day(&self) -> u32 {
        self.hour as u32 * 3600 + self.minute as u32 * 60 + self.second as u32
    }
}

/// Now, in the machine's time zone.
#[cfg(unix)]
pub fn now() -> LocalTime {
    // SAFETY: `time` and `localtime_r` with a zeroed `tm` of our own; no shared state.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        LocalTime {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u8,
            day: tm.tm_mday as u8,
            hour: tm.tm_hour as u8,
            minute: tm.tm_min as u8,
            second: tm.tm_sec.min(59) as u8,
            weekday: tm.tm_wday as u8,
        }
    }
}

#[cfg(windows)]
#[repr(C)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLocalTime(st: *mut SystemTime);
}

/// Now, in the machine's time zone.
#[cfg(windows)]
pub fn now() -> LocalTime {
    let mut st = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    // SAFETY: GetLocalTime fills the struct we own.
    unsafe { GetLocalTime(&mut st) };
    LocalTime {
        year: st.year as i32,
        month: st.month as u8,
        day: st.day as u8,
        hour: st.hour as u8,
        minute: st.minute as u8,
        second: st.second as u8,
        weekday: st.day_of_week as u8,
    }
}

impl LocalTime {
    /// As a file or folder name: `2026-10-29 14.05`.
    pub fn stamp(&self) -> String {
        format!(
            "{}-{:02}-{:02} {:02}.{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn now_is_plausible() {
        let t = super::now();
        assert!(t.year >= 2024 && (1..=12).contains(&t.month) && (1..=31).contains(&t.day));
        assert!(t.hour < 24 && t.minute < 60 && t.weekday < 7);
        assert_eq!(t.stamp().len(), 16);
    }
}
