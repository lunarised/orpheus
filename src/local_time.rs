use std::time::{SystemTime, UNIX_EPOCH};

/// A validated snapshot of the system's local civil time.
///
/// Keeping the libc boundary here gives the rest of the application a safe,
/// platform-sized representation instead of repeating `localtime_r` calls and
/// unchecked integer casts in each UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub weekday: u8,
    pub year_day: u16,
}

impl LocalDateTime {
    pub fn now() -> Option<Self> {
        let seconds = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        Self::from_unix(seconds)
    }

    pub fn from_unix(seconds: u64) -> Option<Self> {
        let timestamp = libc::time_t::try_from(seconds).ok()?;
        let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: both pointers remain valid for the call. `localtime_r`
        // initializes `local` before returning a non-null pointer.
        let result = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) };
        if result.is_null() {
            return None;
        }
        // SAFETY: the successful call above initialized the entire `tm`.
        let local = unsafe { local.assume_init() };

        let year = local.tm_year.checked_add(1900)?;
        let month = u8::try_from(local.tm_mon.checked_add(1)?).ok()?;
        let day = u8::try_from(local.tm_mday).ok()?;
        let hour = u8::try_from(local.tm_hour).ok()?;
        let minute = u8::try_from(local.tm_min).ok()?;
        let weekday = u8::try_from(local.tm_wday).ok()?;
        let year_day = u16::try_from(local.tm_yday).ok()?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || weekday > 6
            || year_day > 365
        {
            return None;
        }

        Some(Self {
            year,
            month,
            day,
            hour,
            minute,
            weekday,
            year_day,
        })
    }

    /// A compact key that changes once per local calendar day.
    pub fn day_key(self) -> i32 {
        self.year
            .saturating_mul(400)
            .saturating_add(i32::from(self.year_day))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_time_is_converted_into_valid_local_fields() {
        let local = LocalDateTime::from_unix(0).unwrap();

        assert!((1..=12).contains(&local.month));
        assert!((1..=31).contains(&local.day));
        assert!(local.hour <= 23);
        assert!(local.minute <= 59);
        assert!(local.weekday <= 6);
        assert!(local.year_day <= 365);
    }

    #[test]
    fn timestamps_that_do_not_fit_time_t_are_rejected() {
        assert_eq!(LocalDateTime::from_unix(u64::MAX), None);
    }
}
