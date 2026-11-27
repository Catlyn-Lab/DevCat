//! Ending everything a terminal started: a closed tab mustn't leave its program's helpers running
//! (Claude Code's MCP servers, a dev server started from the shell, a build).
//!
//! On macOS and Linux a terminal's program leads its own session (portable-pty calls `setsid`), and
//! what it starts stays in that session unless it leaves on purpose. So a terminal's processes are
//! the ones in its session, plus what those started while they're still their parent. A daemon
//! that left the session and lost its parent is neither and keeps running: ssh's shared master
//! connection (`ControlPersist`, `remote::ssh_args`), which other tabs and file commands use, or a
//! tmux server.
//!
//! On Windows the program's process tree is ended with `taskkill /T` while the program still runs;
//! what outlives it there isn't found.
#[cfg(unix)]
use std::time::{Duration, Instant};

/// A process seen in a terminal's session. Its session is checked again before every signal, so a
/// pid given to another process meanwhile is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pid: i32,
    #[cfg(unix)]
    sid: i32,
}

/// Every process: (pid, parent pid).
#[cfg(unix)]
fn table() -> Vec<(i32, i32)> {
    let Ok(out) = std::process::Command::new("/bin/ps").args(["-A", "-o", "pid=,ppid="]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(str::parse::<i32>);
            Some((f.next()?.ok()?, f.next()?.ok()?))
        })
        .collect()
}

#[cfg(unix)]
fn sid_of(pid: i32) -> i32 {
    // SAFETY: a plain system call; -1 when the process is gone.
    unsafe { libc::getsid(pid) }
}

/// The processes of the session `leader` leads (its own pid while it runs), and what they started.
/// Never DevCat itself.
#[cfg(unix)]
pub fn of_session(leader: u32) -> Vec<Proc> {
    let leader = leader as i32;
    if leader <= 1 {
        return vec![];
    }
    let me = std::process::id() as i32;
    let rows = table();
    members(leader, me, &rows, sid_of)
        .into_iter()
        .filter_map(|pid| {
            let sid = sid_of(pid);
            (sid > 0).then_some(Proc { pid, sid })
        })
        .collect()
}

/// `of_session` over a process table, with `sid` telling each process's session.
#[cfg(unix)]
fn members(leader: i32, me: i32, rows: &[(i32, i32)], sid: impl Fn(i32) -> i32) -> Vec<i32> {
    let mut found: Vec<i32> = rows.iter().map(|r| r.0).filter(|&p| p > 1 && p != me && sid(p) == leader).collect();
    // what they started, also when it left the session (a setsid'd child whose parent is still here)
    loop {
        let more: Vec<i32> = rows.iter().filter(|(p, pp)| *p > 1 && *p != me && found.contains(pp) && !found.contains(p)).map(|r| r.0).collect();
        if more.is_empty() {
            return found;
        }
        found.extend(more);
    }
}

#[cfg(not(unix))]
pub fn of_session(_leader: u32) -> Vec<Proc> {
    vec![]
}

/// The ones still there (same pid, same session).
#[cfg(unix)]
fn alive(procs: &[Proc]) -> Vec<Proc> {
    procs.iter().copied().filter(|p| sid_of(p.pid) == p.sid).collect()
}

#[cfg(unix)]
fn signal(procs: &[Proc], sig: i32) {
    for p in alive(procs) {
        // SAFETY: a plain signal to a process checked just now to be in the terminal's session.
        unsafe {
            libc::kill(p.pid, sig);
        }
    }
}

/// End `procs`: the hangup a closing terminal gives and SIGTERM (an interactive shell ignores
/// SIGTERM, a server may ignore SIGHUP), then SIGKILL for whatever is still there after `grace`.
/// Blocks for up to `grace`.
#[cfg(unix)]
pub fn end(procs: &[Proc], grace: Duration) {
    if procs.is_empty() {
        return;
    }
    signal(procs, libc::SIGHUP);
    signal(procs, libc::SIGTERM);
    let until = Instant::now() + grace;
    while Instant::now() < until {
        if alive(procs).is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    signal(procs, libc::SIGKILL);
}

#[cfg(not(unix))]
pub fn end(_procs: &[Proc], _grace: std::time::Duration) {}

/// Windows: end `pid` and the processes it started (while it still runs: the tree is followed
/// through it).
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn members_are_the_session_and_what_it_started() {
        // 100 leads the terminal's session; 101 and 102 are in it; 103 left it (setsid) but its
        // parent 101 is still there; 104 is 103's child; 200 is another session; 300 a detached
        // daemon (ssh's master) whose parent is launchd; 50 is DevCat.
        let rows = [(100, 50), (101, 100), (102, 101), (103, 101), (104, 103), (200, 1), (201, 200), (300, 1), (50, 1)];
        let sid = |p: i32| match p {
            100..=102 => 100,
            103 | 104 => 103,
            200 | 201 => 200,
            300 => 300,
            _ => 1,
        };
        let mut got = members(100, 50, &rows, sid);
        got.sort();
        assert_eq!(got, vec![100, 101, 102, 103, 104]);
    }

    #[test]
    fn never_devcat_or_launchd() {
        // even when DevCat or pid 1 would appear to be in the session
        let rows = [(1, 0), (50, 1), (100, 50), (101, 50)];
        let mut got = members(100, 50, &rows, |_| 100);
        got.sort();
        assert_eq!(got, vec![100, 101]);
    }

    #[test]
    fn ends_a_whole_session_including_what_ignores_sigterm() {
        use std::os::unix::process::CommandExt;
        // a leader with its own session, a child that ignores SIGHUP and SIGTERM, and a grandchild
        let mut leader = std::process::Command::new("/bin/sh");
        leader.args(["-c", "sh -c 'trap \"\" HUP TERM; sleep 60 & wait' & sleep 60 & wait"]);
        // SAFETY: setsid in the child before exec, as portable-pty does.
        unsafe {
            leader.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = leader.spawn().unwrap();
        let pid = child.id();
        let until = Instant::now() + Duration::from_secs(5);
        let mut procs = of_session(pid);
        while procs.len() < 4 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
            procs = of_session(pid);
        }
        assert!(procs.len() >= 4, "found {procs:?}");
        end(&procs, Duration::from_millis(500));
        let _ = child.wait();
        let until = Instant::now() + Duration::from_secs(2);
        while !alive(&procs).is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(alive(&procs).is_empty(), "still running: {:?}", alive(&procs));
        assert!(of_session(pid).is_empty());
    }
}
