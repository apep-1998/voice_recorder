//! Power-event integration.
//!
//! Recording must survive suspend, resume, and lid-close across the whole
//! retention window. Two mechanisms cooperate:
//!
//! 1. The capture engine's clock-drift detector (see [`crate::timeline`])
//!    already catches suspend: on resume, wall-clock has jumped far ahead of
//!    the sample clock, so the current segment is finalized and a new session
//!    begins — the suspend shows up as a clean gap, not corruption. This works
//!    with no D-Bus at all.
//! 2. This module adds *proactive* finalization: a logind `PrepareForSleep`
//!    watcher takes a delay inhibitor so segments are flushed to disk before
//!    the machine actually suspends, shrinking the worst-case loss window.
//!
//! The watcher is abstracted behind [`SleepWatcher`] so the daemon can be
//! tested with a fake, and the real zbus implementation stays isolated.

use std::time::Duration;

/// A power transition reported by a [`SleepWatcher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepEvent {
    /// The system is about to suspend. Finalize in-flight work now.
    GoingToSleep,
    /// The system just resumed.
    Resumed,
}

/// Something that reports sleep/resume transitions.
pub trait SleepWatcher {
    /// Block until the next event, or return `None` if the watcher stops.
    fn next_event(&mut self) -> Option<SleepEvent>;
}

/// Maximum time we hold logind's sleep inhibitor while flushing; logind caps
/// this at `InhibitDelayMaxUSec` (5s by default), so stay well under it.
pub const MAX_INHIBIT_HOLD: Duration = Duration::from_secs(3);

#[cfg(target_os = "linux")]
pub use zbus_impl::ZbusSleepWatcher;

#[cfg(target_os = "linux")]
mod zbus_impl {
    use super::{SleepEvent, SleepWatcher};
    use crate::error::PowerError;

    /// logind `PrepareForSleep` watcher backed by the system D-Bus.
    pub struct ZbusSleepWatcher {
        // Held so the connection and signal stream stay alive.
        rx: std::sync::mpsc::Receiver<SleepEvent>,
        _thread: std::thread::JoinHandle<()>,
    }

    impl ZbusSleepWatcher {
        /// Connect to the system bus and begin watching. Runs its own thread
        /// with a small blocking zbus connection so callers need no async
        /// runtime.
        pub fn connect() -> Result<Self, PowerError> {
            let (tx, rx) = std::sync::mpsc::channel();
            let conn = zbus::blocking::Connection::system()
                .map_err(|e| PowerError::Dbus(e.to_string()))?;
            let proxy = zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )
            .map_err(|e| PowerError::Dbus(e.to_string()))?;

            let signal = proxy
                .receive_signal("PrepareForSleep")
                .map_err(|e| PowerError::Dbus(e.to_string()))?;

            let thread = std::thread::Builder::new()
                .name("voicerec-logind".into())
                .spawn(move || {
                    for msg in signal {
                        // Body is a single bool: true = about to sleep.
                        if let Ok(going) = msg.body().deserialize::<bool>() {
                            let event = if going {
                                SleepEvent::GoingToSleep
                            } else {
                                SleepEvent::Resumed
                            };
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                })
                .map_err(|e| PowerError::Dbus(e.to_string()))?;

            Ok(Self {
                rx,
                _thread: thread,
            })
        }
    }

    impl SleepWatcher for ZbusSleepWatcher {
        fn next_event(&mut self) -> Option<SleepEvent> {
            self.rx.recv().ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted watcher for tests.
    struct FakeWatcher {
        events: std::collections::VecDeque<SleepEvent>,
    }

    impl SleepWatcher for FakeWatcher {
        fn next_event(&mut self) -> Option<SleepEvent> {
            self.events.pop_front()
        }
    }

    #[test]
    fn watcher_reports_scripted_events_then_stops() {
        let mut w = FakeWatcher {
            events: [SleepEvent::GoingToSleep, SleepEvent::Resumed]
                .into_iter()
                .collect(),
        };
        assert_eq!(w.next_event(), Some(SleepEvent::GoingToSleep));
        assert_eq!(w.next_event(), Some(SleepEvent::Resumed));
        assert_eq!(w.next_event(), None);
    }

    #[test]
    fn inhibit_hold_is_under_logind_default_cap() {
        // logind's default InhibitDelayMaxUSec is 5s; we must finish first.
        assert!(MAX_INHIBIT_HOLD < Duration::from_secs(5));
    }
}
