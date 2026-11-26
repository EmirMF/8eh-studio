use std::time::{Duration, Instant};

const IDLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const BROADCAST_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct SessionChecks {
    target: Option<(String, String)>,
    running: bool,
    next_check: Option<Instant>,
    in_flight: Option<(String, String)>,
}

impl SessionChecks {
    pub(crate) fn begin(
        &mut self,
        now: Instant,
        url: &str,
        token: &str,
        running: bool,
        immediate: bool,
    ) -> bool {
        if token.is_empty() || url.is_empty() {
            self.target = None;
            self.next_check = None;
            return false;
        }

        let interval = if running {
            BROADCAST_INTERVAL
        } else {
            IDLE_INTERVAL
        };
        let target_changed = !self
            .target
            .as_ref()
            .is_some_and(|(site, key)| site == url && key == token);
        if target_changed {
            self.target = Some((url.to_owned(), token.to_owned()));
        }
        if target_changed || self.running != running {
            self.running = running;
            self.next_check = Some(now + interval);
        }

        if let Some((site, key)) = &self.in_flight {
            if immediate && (site != url || key != token) {
                self.next_check = Some(now);
            }
            return false;
        }
        if !immediate && self.next_check.is_some_and(|due| now < due) {
            return false;
        }

        self.next_check = Some(now + interval);
        self.in_flight.clone_from(&self.target);
        true
    }

    pub(crate) fn finish(&mut self) {
        self.in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn startup_then_five_minute_idle_checks_even_after_failure() {
        let start = Instant::now();
        let mut checks = SessionChecks::default();
        assert!(checks.begin(start, "site", "token", false, true));
        checks.finish();
        assert!(!checks.begin(at(start, 299), "site", "token", false, false));
        assert!(checks.begin(at(start, 300), "site", "token", false, false));
        checks.finish();
        assert!(!checks.begin(at(start, 599), "site", "token", false, false));
        assert!(checks.begin(at(start, 600), "site", "token", false, false));
    }

    #[test]
    fn starts_a_new_interval_on_each_broadcast_transition() {
        let start = Instant::now();
        let mut checks = SessionChecks::default();
        assert!(checks.begin(start, "site", "token", false, true));
        checks.finish();
        assert!(!checks.begin(at(start, 270), "site", "token", true, false));
        assert!(!checks.begin(at(start, 299), "site", "token", true, false));
        assert!(checks.begin(at(start, 300), "site", "token", true, false));
        checks.finish();
        assert!(!checks.begin(at(start, 330), "site", "token", false, false));
        assert!(!checks.begin(at(start, 629), "site", "token", false, false));
        assert!(checks.begin(at(start, 630), "site", "token", false, false));
    }

    #[test]
    fn coalesces_checks_and_skips_unlinked_devices() {
        let start = Instant::now();
        let mut checks = SessionChecks::default();
        assert!(!checks.begin(start, "site", "", false, true));
        assert!(checks.begin(start, "site", "old", false, true));
        assert!(!checks.begin(at(start, 1), "site", "old", false, true));
        assert!(!checks.begin(at(start, 2), "site", "new", false, false));
        assert!(!checks.begin(at(start, 3), "site", "new", false, true));
        checks.finish();
        assert!(checks.begin(at(start, 4), "site", "new", false, false));
        checks.finish();
        assert!(!checks.begin(at(start, 303), "site", "", false, false));
        assert!(!checks.begin(at(start, 304), "site", "linked", false, false));
    }
}
