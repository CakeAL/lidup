//! One display request per worker turn; retries never sleep or outlive their context.
use crate::displays::DisplayInfo;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Unplug,
    Angle,
    Manual,
    Quit,
}

/// The worker's settled observation. A missing built-in is software-disabled;
/// an asleep panel must never receive a recovery request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinState {
    Awake,
    Off,
    Asleep,
}

impl From<Option<&DisplayInfo>> for BuiltinState {
    fn from(display: Option<&DisplayInfo>) -> Self {
        match display {
            Some(display) if display.asleep => Self::Asleep,
            Some(display) if display.on => Self::Awake,
            _ => Self::Off,
        }
    }
}

struct Task {
    reason: Reason,
    next: Instant,
    expires: Instant,
    attempts: u8,
    last_error: Option<String>,
}

#[derive(Default)]
pub struct Recovery {
    task: Option<Task>,
}

impl Recovery {
    pub fn reason(&self) -> Option<Reason> {
        self.task.as_ref().map(|task| task.reason)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.task.as_ref().map(|task| task.next.min(task.expires))
    }

    pub fn request(&mut self, reason: Reason, now: Instant) {
        if self.reason() == Some(reason) {
            return;
        }
        self.task = Some(Task {
            reason,
            next: now,
            expires: now + Duration::from_secs(5),
            attempts: 0,
            last_error: None,
        });
    }

    pub fn cancel(&mut self) {
        self.task = None;
    }

    /// A configuration callback makes the old snapshot invalid. Let the worker
    /// revalidate the purpose against a settled snapshot before another request.
    pub fn defer(&mut self, until: Instant) {
        if let Some(task) = &mut self.task {
            task.next = task.next.max(until);
        }
    }

    pub fn step(
        &mut self,
        now: Instant,
        builtin: BuiltinState,
        mut enable: impl FnMut(bool) -> Result<(), String>,
    ) -> Result<Option<Instant>, String> {
        if builtin == BuiltinState::Asleep {
            self.cancel();
            return Ok(None);
        }
        let Some(task) = &mut self.task else {
            return Ok(None);
        };
        // The first forced request is necessary after unplug even when CG says
        // the built-in is active while its panel is still dark.
        if now < task.next && now < task.expires {
            return Ok(self.deadline());
        }
        if now >= task.next && task.attempts > 0 && builtin == BuiltinState::Awake {
            self.cancel();
            return Ok(None);
        }
        if now >= task.expires {
            let error = task
                .last_error
                .take()
                .unwrap_or_else(|| "未在 5 秒内确认内建屏恢复".into());
            self.cancel();
            return Err(error);
        }
        let permanent = task.reason != Reason::Angle || task.attempts >= 3;
        if let Err(error) = enable(permanent) {
            crate::diagnostics::record(&format!("builtin recovery request failed: {error}"));
            task.last_error = Some(error);
        }
        task.attempts += 1;
        task.next = now + Duration::from_millis(300);
        Ok(self.deadline())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_callback_cannot_confirm_transient_active_state() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        recovery.request(Reason::Unplug, now);
        recovery.step(now, BuiltinState::Off, |_| Ok(())).unwrap();
        recovery.defer(now + Duration::from_millis(700));
        assert_eq!(
            recovery.step(
                now + Duration::from_millis(300),
                BuiltinState::Awake,
                |_| panic!("unsettled display")
            ),
            Ok(Some(now + Duration::from_millis(700)))
        );
        assert_eq!(recovery.reason(), Some(Reason::Unplug));
        // The apparent activation was rolled back. Recovery must still retry.
        let mut requests = 0;
        recovery
            .step(now + Duration::from_millis(700), BuiltinState::Off, |_| {
                requests += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(requests, 1);
    }

    #[test]
    fn sleeping_builtin_cancels_every_recovery_purpose_without_configuration() {
        let now = Instant::now();
        for reason in [Reason::Unplug, Reason::Angle, Reason::Manual, Reason::Quit] {
            let mut recovery = Recovery::default();
            recovery.request(reason, now);
            assert_eq!(
                recovery.step(now, BuiltinState::Asleep, |_| panic!("sleeping panel")),
                Ok(None)
            );
            assert_eq!(recovery.deadline(), None);
        }
    }

    #[test]
    fn cancellation_prevents_stale_retry_and_new_request_gets_fresh_window() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        recovery.request(Reason::Unplug, now);
        recovery.step(now, BuiltinState::Off, |_| Ok(())).unwrap();
        recovery.cancel(); // sleep, manual override, or reconnected monitor
        assert_eq!(
            recovery.step(now + Duration::from_secs(1), BuiltinState::Off, |_| panic!(
                "stale request"
            )),
            Ok(None)
        );
        let wake = now + Duration::from_secs(3600);
        recovery.request(Reason::Unplug, wake);
        assert!(recovery
            .step(wake, BuiltinState::Off, |_| Ok(()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn callback_defers_retry_and_repeated_request_cannot_extend_timeout() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        recovery.request(Reason::Unplug, now);
        recovery.step(now, BuiltinState::Awake, |_| Ok(())).unwrap();
        recovery.defer(now + Duration::from_millis(700));
        recovery.request(Reason::Unplug, now + Duration::from_secs(1));
        assert_eq!(
            recovery.step(
                now + Duration::from_millis(300),
                BuiltinState::Off,
                |_| panic!("unsettled display")
            ),
            Ok(Some(now + Duration::from_millis(700)))
        );
        assert!(recovery
            .step(now + Duration::from_secs(5), BuiltinState::Off, |_| panic!(
                "expired request"
            ))
            .is_err());
        assert_eq!(recovery.deadline(), None);
    }

    #[test]
    fn angle_tries_session_before_permanent_and_stops_on_confirmation() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        recovery.request(Reason::Angle, now);
        let mut requests = Vec::new();
        for millis in [0, 300, 600, 900] {
            recovery
                .step(
                    now + Duration::from_millis(millis),
                    BuiltinState::Off,
                    |permanent| {
                        requests.push(permanent);
                        Ok(())
                    },
                )
                .unwrap();
        }
        assert_eq!(requests, [false, false, false, true]);
        assert_eq!(
            recovery.step(
                now + Duration::from_millis(1200),
                BuiltinState::Awake,
                |_| panic!("already online")
            ),
            Ok(None)
        );
    }
}
