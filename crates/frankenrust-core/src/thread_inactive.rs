//! Handler for a PHP thread which currently has no assigned work.

use std::fmt;
use std::io::{self, Write};

use crate::state::State;
use crate::thread::{PhpThread, ScriptPath, ThreadHandler};

/// Keeping an inactive interpreter parked costs memory but lets a later
/// handler transition reuse its initialized TSRM state immediately.
pub(crate) struct InactiveThread;

impl ThreadHandler for InactiveThread {
    fn name(&self) -> &str {
        "Inactive PHP Thread"
    }

    fn before_script_execution(&self, thread: &PhpThread) -> Option<ScriptPath> {
        // Upstream writes this state machine as tail recursion
        // (`threadinactive.go:21-49`). A PHP pthread has a fixed native stack,
        // so keep every transition in one frame.
        loop {
            match thread.state_machine().get() {
                State::TransitionRequested => return thread.transition_to_new_handler(),
                State::Booting | State::TransitionComplete | State::Inactive => {
                    thread.state_machine().set(State::Inactive);
                    thread.state_machine().mark_as_waiting(true);
                    thread.state_machine().wait_for(&[
                        State::TransitionRequested,
                        State::ShuttingDown,
                        State::Rebooting,
                        State::ForceRebooting,
                    ]);
                    thread.state_machine().mark_as_waiting(false);
                }
                State::Rebooting | State::ForceRebooting | State::ShuttingDown => return None,
                State::RebootReady => thread.state_machine().set(State::Inactive),
                unexpected => {
                    // Upstream panics here. Panicking through the C callback
                    // would abort the process, while returning NULL would
                    // self-retire the slot into Done with no controller to
                    // restore Reserved. Preserve the slot and wait for the
                    // controller to publish a state this handler owns.
                    write_diagnostic(format_args!(
                        "inactive PHP thread {} observed unexpected state {unexpected}",
                        thread.index()
                    ));
                    thread.state_machine().wait_for(&[
                        State::Booting,
                        State::TransitionRequested,
                        State::TransitionComplete,
                        State::Inactive,
                        State::ShuttingDown,
                        State::Rebooting,
                        State::ForceRebooting,
                        State::RebootReady,
                    ]);
                }
            }
        }
    }

    fn after_script_execution(&self, _thread: &PhpThread, _exit_status: i32) {
        // This handler only returns NULL. C enters the script loop, and thus
        // calls after-script, only for a non-NULL path (`frankenphp.c:1506-1562`).
    }
}

fn write_diagnostic(arguments: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "frankenrust: {arguments}");
}
