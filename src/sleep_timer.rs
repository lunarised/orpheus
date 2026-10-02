use std::time::{Duration, Instant};

/// The final minute of a sleep timer gently fades the shared player volume.
pub const FADE_DURATION: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepTimerTick {
    pub volume: Option<u8>,
    pub expired: bool,
}

/// A wall-clock playback timer with a non-destructive volume fade.
///
/// `base_volume` is restored after the player has been paused or when the
/// timer is cancelled, so the next listening session starts at the volume the
/// listener selected rather than at the tail end of the fade.
#[derive(Debug)]
pub struct SleepTimer {
    deadline: Instant,
    duration: Duration,
    base_volume: u8,
    fade_anchor_volume: u8,
    fade_anchor_remaining: Duration,
    last_fade_volume: Option<u8>,
}

impl SleepTimer {
    pub fn new(duration: Duration, base_volume: u8, now: Instant) -> Self {
        Self {
            deadline: now.checked_add(duration).unwrap_or(now),
            duration,
            base_volume,
            fade_anchor_volume: base_volume,
            fade_anchor_remaining: duration.min(FADE_DURATION),
            last_fade_volume: None,
        }
    }

    pub fn remaining_seconds(&self, now: Instant) -> u64 {
        let remaining = self.deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            0
        } else {
            // Round up so a newly started 15-minute timer displays 15:00,
            // rather than immediately appearing to have lost a second.
            remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() > 0))
        }
    }

    pub fn is_fading(&self, now: Instant) -> bool {
        let fade_duration = self.duration.min(FADE_DURATION);
        now >= self
            .deadline
            .checked_sub(fade_duration)
            .unwrap_or(self.deadline)
            && now < self.deadline
    }

    pub fn base_volume(&self) -> u8 {
        self.base_volume
    }

    /// Rebase an active timer after an explicit listener volume change.
    pub fn set_base_volume(&mut self, volume: u8, now: Instant) {
        self.base_volume = volume;
        self.fade_anchor_volume = volume;
        self.fade_anchor_remaining = if self.is_fading(now) {
            self.deadline.saturating_duration_since(now)
        } else {
            self.duration.min(FADE_DURATION)
        };
        self.last_fade_volume = None;
    }

    pub fn tick(&mut self, now: Instant) -> SleepTimerTick {
        if now >= self.deadline {
            return SleepTimerTick {
                volume: None,
                expired: true,
            };
        }

        if !self.is_fading(now) || self.base_volume == 0 {
            return SleepTimerTick {
                volume: None,
                expired: false,
            };
        }

        let remaining = self.deadline.saturating_duration_since(now);
        let fade_nanos = self.fade_anchor_remaining.as_nanos().max(1);
        let numerator = u128::from(self.fade_anchor_volume).saturating_mul(remaining.as_nanos());
        let volume = numerator
            .div_ceil(fade_nanos)
            .clamp(1, u128::from(self.fade_anchor_volume)) as u8;
        let changed = self.last_fade_volume != Some(volume);
        self.last_fade_volume = Some(volume);

        SleepTimerTick {
            volume: changed.then_some(volume),
            expired: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_down_and_only_fades_during_the_final_minute() {
        let start = Instant::now();
        let mut timer = SleepTimer::new(Duration::from_secs(15 * 60), 60, start);

        assert_eq!(timer.remaining_seconds(start), 15 * 60);
        assert!(!timer.is_fading(start + Duration::from_secs(13 * 60)));
        assert_eq!(
            timer.tick(start + Duration::from_secs(13 * 60)),
            SleepTimerTick {
                volume: None,
                expired: false
            }
        );

        assert!(timer.is_fading(start + Duration::from_secs(14 * 60 + 30)));
        assert_eq!(
            timer.tick(start + Duration::from_secs(14 * 60 + 30)).volume,
            Some(30)
        );
        assert_eq!(
            timer.tick(start + Duration::from_secs(14 * 60 + 30)).volume,
            None
        );
    }

    #[test]
    fn expiry_and_short_timers_are_deterministic() {
        let start = Instant::now();
        let mut timer = SleepTimer::new(Duration::from_secs(30), 50, start);

        assert!(timer.is_fading(start));
        assert_eq!(timer.tick(start).volume, Some(50));
        assert_eq!(timer.tick(start + Duration::from_secs(15)).volume, Some(25));
        assert!(timer.tick(start + Duration::from_secs(30)).expired);
        assert_eq!(timer.remaining_seconds(start + Duration::from_secs(30)), 0);
    }

    #[test]
    fn manual_volume_changes_rebase_the_fade_and_restore_target() {
        let start = Instant::now();
        let mut timer = SleepTimer::new(Duration::from_secs(60), 80, start);
        assert_eq!(timer.tick(start + Duration::from_secs(30)).volume, Some(40));

        timer.set_base_volume(30, start + Duration::from_secs(30));

        assert_eq!(timer.base_volume(), 30);
        assert_eq!(timer.tick(start + Duration::from_secs(30)).volume, Some(30));
        assert_eq!(timer.tick(start + Duration::from_secs(45)).volume, Some(15));
    }
}
