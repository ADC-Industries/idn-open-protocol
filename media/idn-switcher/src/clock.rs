//! The machine's local time, for titles' clocks and countdowns to a time of day (§S.19):
//! the library's (`idn_open_protocol::localtime`).

pub use idn_open_protocol::localtime::LocalTime;

/// Now, in the machine's time zone.
pub fn local_now() -> LocalTime {
    idn_open_protocol::localtime::now()
}

/// Seconds as `m:ss`, or `h:mm:ss` from an hour.
pub fn duration_text(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_well() {
        assert_eq!(duration_text(0), "0:00");
        assert_eq!(duration_text(65), "1:05");
        assert_eq!(duration_text(3600 + 61), "1:01:01");
    }
}
