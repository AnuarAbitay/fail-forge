//! Ask Windows to keep running while a long command runs.
//!
//! Campaigns last tens of minutes and laptops sleep after a few idle minutes
//! by default. A power request keeps the system out of idle sleep, including
//! Modern Standby (S0 low-power idle), where a thread execution state does not
//! stop standby entry once the display turns off. A user can still sleep the
//! machine (lid, power button); command and adapter timeouts then count only
//! awake time, see `failforge_runner::AwakeInstant`.

/// Reported wall times include system sleep. Name it when it was long enough
/// to matter, so a slow-looking run is not mistaken for slow work.
pub fn sleep_note(suspended: std::time::Duration) -> Option<String> {
    (suspended.as_secs() >= 1).then(|| {
        format!(
            "failforge: the system slept for {}s during this run; wall times include it\n",
            suspended.as_secs()
        )
    })
}

/// Holds the power request until dropped.
pub struct KeepAwake {
    #[cfg(windows)]
    _held: Option<windows::Held>,
}

impl KeepAwake {
    pub fn request() -> Self {
        #[cfg(windows)]
        {
            Self {
                _held: windows::Held::new(&windows::SYSTEM),
            }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Power::{
        POWER_REQUEST_TYPE, PowerClearRequest, PowerCreateRequest, PowerRequestExecutionRequired,
        PowerRequestSystemRequired, PowerSetRequest,
    };
    use windows_sys::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };

    /// SystemRequired keeps the system out of idle sleep; ExecutionRequired
    /// keeps this process and its children running in Modern Standby.
    pub(super) const KINDS: [POWER_REQUEST_TYPE; 2] =
        [PowerRequestSystemRequired, PowerRequestExecutionRequired];

    /// The power-request calls, replaceable in tests.
    pub(super) struct Api {
        pub create: fn() -> HANDLE,
        pub set: fn(HANDLE, POWER_REQUEST_TYPE) -> bool,
        pub clear: fn(HANDLE, POWER_REQUEST_TYPE) -> bool,
        pub close: fn(HANDLE),
    }

    pub(super) static SYSTEM: Api = Api {
        create: || {
            let mut reason: Vec<u16> = "FailForge mutation run\0".encode_utf16().collect();
            let context = REASON_CONTEXT {
                // POWER_REQUEST_CONTEXT_VERSION
                Version: 0,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    SimpleReasonString: reason.as_mut_ptr(),
                },
            };
            // SAFETY: the context and its reason string outlive the call,
            // which copies the reason into the new request.
            unsafe { PowerCreateRequest(&context) }
        },
        // SAFETY for the calls below: the handle is a live power request.
        set: |request, kind| unsafe { PowerSetRequest(request, kind) != 0 },
        clear: |request, kind| unsafe { PowerClearRequest(request, kind) != 0 },
        close: |request| unsafe {
            CloseHandle(request);
        },
    };

    /// A power request and the request types it holds.
    pub(super) struct Held {
        request: HANDLE,
        kinds: Vec<POWER_REQUEST_TYPE>,
        api: &'static Api,
    }

    impl Held {
        /// A failed request leaves sleep possible; awake-time timeouts still
        /// keep the run correct, so the command proceeds without it.
        pub(super) fn new(api: &'static Api) -> Option<Self> {
            let request = (api.create)();
            if request.is_null() || request == INVALID_HANDLE_VALUE {
                return None;
            }
            let kinds = KINDS
                .into_iter()
                .filter(|kind| (api.set)(request, *kind))
                .collect();
            Some(Self {
                request,
                kinds,
                api,
            })
        }

        #[cfg(test)]
        pub(super) fn kinds(&self) -> &[POWER_REQUEST_TYPE] {
            &self.kinds
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            for kind in &self.kinds {
                (self.api.clear)(self.request, *kind);
            }
            (self.api.close)(self.request);
        }
    }
}

#[cfg(test)]
mod note_tests {
    use super::sleep_note;
    use std::time::Duration;

    #[test]
    fn sleep_is_noted_from_one_second() {
        assert_eq!(sleep_note(Duration::ZERO), None);
        assert_eq!(sleep_note(Duration::from_millis(999)), None);
        assert_eq!(
            sleep_note(Duration::from_secs(1)).as_deref(),
            Some("failforge: the system slept for 1s during this run; wall times include it\n")
        );
        assert!(
            sleep_note(Duration::from_secs(10_831))
                .unwrap()
                .contains("slept for 10831s")
        );
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::windows::{Api, Held, KINDS, SYSTEM};
    use super::*;
    use std::cell::RefCell;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Power::{
        PowerRequestExecutionRequired, PowerRequestSystemRequired,
    };

    #[derive(Debug, PartialEq)]
    enum Call {
        Create,
        Set(i32),
        Clear(i32),
        Close,
    }

    thread_local! {
        static CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
    }

    fn record(call: Call) {
        CALLS.with(|calls| calls.borrow_mut().push(call));
    }

    fn calls() -> Vec<Call> {
        CALLS.with(|calls| calls.borrow_mut().drain(..).collect())
    }

    const REQUEST: HANDLE = 0x40 as HANDLE;

    static ACCEPTING: Api = Api {
        create: || {
            record(Call::Create);
            REQUEST
        },
        set: |request, kind| {
            assert_eq!(request, REQUEST);
            record(Call::Set(kind));
            true
        },
        clear: |request, kind| {
            assert_eq!(request, REQUEST);
            record(Call::Clear(kind));
            true
        },
        close: |request| {
            assert_eq!(request, REQUEST);
            record(Call::Close);
        },
    };

    static NO_EXECUTION: Api = Api {
        set: |request, kind| {
            record(Call::Set(kind));
            request == REQUEST && kind == PowerRequestSystemRequired
        },
        ..ACCEPTING
    };

    static FAILED_CREATE: Api = Api {
        create: || {
            record(Call::Create);
            INVALID_HANDLE_VALUE
        },
        ..ACCEPTING
    };

    #[test]
    fn requests_are_held_until_dropped_and_then_cleared_and_closed() {
        let held = Held::new(&ACCEPTING).unwrap();
        assert_eq!(held.kinds(), KINDS);
        assert_eq!(
            calls(),
            [
                Call::Create,
                Call::Set(PowerRequestSystemRequired),
                Call::Set(PowerRequestExecutionRequired)
            ]
        );
        drop(held);
        assert_eq!(
            calls(),
            [
                Call::Clear(PowerRequestSystemRequired),
                Call::Clear(PowerRequestExecutionRequired),
                Call::Close
            ]
        );
    }

    #[test]
    fn only_granted_request_types_are_cleared() {
        let held = Held::new(&NO_EXECUTION).unwrap();
        assert_eq!(held.kinds(), [PowerRequestSystemRequired]);
        calls();
        drop(held);
        assert_eq!(
            calls(),
            [Call::Clear(PowerRequestSystemRequired), Call::Close]
        );
    }

    #[test]
    fn a_failed_request_is_not_set_or_closed() {
        assert!(Held::new(&FAILED_CREATE).is_none());
        assert_eq!(calls(), [Call::Create]);
    }

    #[test]
    fn the_system_grants_both_request_types() {
        let held = Held::new(&SYSTEM).expect("power request");
        assert_eq!(held.kinds(), KINDS);
        drop(held);
        assert!(KeepAwake::request()._held.is_some());
    }
}
