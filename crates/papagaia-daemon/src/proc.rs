//! Small process helpers shared by the daemon's child-process supervisors.

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Make `command`'s spawned child receive `SIGKILL` when this process dies, so
/// daemon-owned children (the overlay, the warm whisper-server) can't linger as
/// orphans after the daemon exits.
pub fn die_with_parent(command: &mut Command) {
    // SAFETY: pre_exec runs in the forked child between fork and execve, so it
    // may only call async-signal-safe syscalls — prctl, getppid, and _exit are.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Race: if the parent died between fork and prctl the pdeathsig never
            // fires, so detect an already-orphaned child (reparented to init) and
            // exit instead of lingering.
            if libc::getppid() == 1 {
                libc::_exit(0);
            }
            Ok(())
        });
    }
}
