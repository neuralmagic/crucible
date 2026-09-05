//! Process-wide stop signal and the registry of agent children it has to reach.

use std::sync::atomic::AtomicBool;

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

/// Send SIGTERM to one PID (no-op if zero/negative), via libc rather than the `kill` binary.
pub(crate) fn kill_pid(pid: i32) {
    if pid > 0 {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGTERM);
    }
}

/// Registry of live agent-child PIDs so Ctrl+C can kill ALL concurrent children (wide-round
/// parallel agents and the serial deep-loop agent alike).
pub(crate) mod pid_registry {
    use std::sync::Mutex;

    static PIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    pub fn register(pid: i32) {
        if let Ok(mut v) = PIDS.lock() {
            v.push(pid);
        }
    }

    pub fn deregister(pid: i32) {
        if let Ok(mut v) = PIDS.lock() {
            v.retain(|&p| p != pid);
        }
    }

    pub fn kill_all() {
        if let Ok(v) = PIDS.lock() {
            for &pid in v.iter() {
                crate::process::kill_pid(pid);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn register_and_deregister() {
            register(99999);
            register(99998);
            {
                let v = PIDS.lock().unwrap();
                assert!(v.contains(&99999));
                assert!(v.contains(&99998));
            }
            deregister(99999);
            {
                let v = PIDS.lock().unwrap();
                assert!(!v.contains(&99999));
                assert!(v.contains(&99998));
            }
            deregister(99998);
        }

        #[test]
        fn deregister_nonexistent_is_noop() {
            deregister(77777);
        }
    }
}
