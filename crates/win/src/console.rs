//! Console control-signal observation for graceful cancellation.
//!
//! `SetConsoleCtrlHandler` is process-global, so this module owns one static
//! flag instead of handing out per-caller state. The first Ctrl+C or
//! Ctrl+Break sets the flag and keeps the process alive so the copy engine
//! can stop between chunks and finalize its audit trail (exit 3). A second
//! request, or a close/logoff/shutdown signal, falls through to the default
//! handler and terminates the process — the abort-and-rerun contract already
//! covers that outcome, so a stuck cancellation can always be escalated.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{FALSE, TRUE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};

use crate::util::bool_result;

static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Decides how one console control event is answered.
///
/// Returns Win32 `TRUE` ("handled, keep running") only for the first
/// cancel-class event; everything else defers to the next handler in the
/// chain, which by default terminates the process. Separated from the raw
/// callback so the decision table is unit-testable against a local flag.
fn handle_control_event(flag: &AtomicBool, control_type: u32) -> i32 {
    let cancel = matches!(control_type, CTRL_C_EVENT | CTRL_BREAK_EVENT);
    if cancel && !flag.swap(true, Ordering::AcqRel) {
        TRUE
    } else {
        FALSE
    }
}

unsafe extern "system" fn cancel_handler(control_type: u32) -> i32 {
    // Console control callbacks run on a dedicated thread injected by the
    // console host; touching only a static atomic keeps this re-entrant and
    // allocation-free.
    handle_control_event(&CANCEL_REQUESTED, control_type)
}

/// Installs the process-wide graceful-cancellation console handler.
///
/// After a successful install, the first Ctrl+C/Ctrl+Break makes
/// [`cancel_requested`] return true instead of terminating the process;
/// callers poll it between bounded work units. Installation is idempotent.
/// If it fails, Ctrl+C keeps its default terminate-the-process behavior,
/// which the crash-safety contract treats like any other interruption.
pub fn install_cancel_handler() -> io::Result<()> {
    // SAFETY: `cancel_handler` matches PHANDLER_ROUTINE, stays registered for
    // the process lifetime (it is never removed), and touches only a static
    // atomic, so it is safe to invoke from the console host's callback thread.
    bool_result(unsafe { SetConsoleCtrlHandler(Some(cancel_handler), TRUE) })
}

/// Reports whether a console cancel (Ctrl+C/Ctrl+Break) has been requested.
///
/// The flag is process-wide and latches: once set it stays set, matching the
/// run-level "cancel once, finish safely" model.
#[must_use]
pub fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    use super::handle_control_event;

    #[test]
    fn first_cancel_is_absorbed_and_the_second_escalates() {
        let flag = AtomicBool::new(false);
        assert_eq!(handle_control_event(&flag, CTRL_C_EVENT), TRUE);
        assert!(flag.load(Ordering::Acquire));
        // A second request defers to the default handler (terminate) so a
        // stuck cancellation can always be escalated by the user.
        assert_eq!(handle_control_event(&flag, CTRL_C_EVENT), FALSE);
        assert_eq!(handle_control_event(&flag, CTRL_BREAK_EVENT), FALSE);
    }

    #[test]
    fn close_and_shutdown_signals_are_never_absorbed() {
        let flag = AtomicBool::new(false);
        assert_eq!(handle_control_event(&flag, CTRL_CLOSE_EVENT), FALSE);
        assert_eq!(handle_control_event(&flag, CTRL_SHUTDOWN_EVENT), FALSE);
        // Non-cancel signals must not latch the cancel flag either.
        assert!(!flag.load(Ordering::Acquire));
    }
}
