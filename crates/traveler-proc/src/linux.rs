//! Acquire and capture, Linux.
//!
//! Two ways in, one thing out. Launch is first because `ptrace_scope` is 1 on most distros: a
//! process may only trace its descendants, so a child you started is always traceable while an
//! arbitrary pid is `EPERM` without `CAP_SYS_PTRACE`.
//!
//! The plan sketched `PTRACE_SEIZE` plus `PTRACE_INTERRUPT` for attach and `PTRACE_TRACEME` for
//! launch, which does not survive contact: `PTRACE_INTERRUPT` only works on a tracee that was
//! seized, and a tracee that arrived by `TRACEME` cannot be seized afterwards. Rather than carry
//! two stop paths, everything here freezes with `SIGSTOP` and thaws with `PTRACE_CONT`, which is
//! one mechanism for launched leaders, attached leaders, and helper threads alike.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStdout, Command, Stdio};
use std::time::{Instant, SystemTime};

use crate::{AddressSpace, Caps, CaptureMeta, Error, Result, maps};

/// Cap on stop-and-enumerate rounds. A target spawning threads faster than they can be seized
/// would otherwise spin here; the capture still happens and says it may be torn.
const MAX_SETTLE_ROUNDS: u32 = 8;

#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// Two runs are only comparable with the layout fixed. It changes the program's behaviour,
    /// so it is offered rather than assumed.
    pub disable_aslr: bool,
    pub piped_output: bool,
}

impl Launch {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            ..Default::default()
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn piped(mut self) -> Self {
        self.piped_output = true;
        self
    }
}

/// A process this can read, write, stop, and resume.
pub struct AttachedTarget {
    pid: i32,
    name: String,
    /// Threads seized for the current stop. Never holds the leader.
    helpers: Vec<i32>,
    stopped: bool,
    /// A signal observed instead of our own `SIGSTOP`, re-delivered on resume.
    pending: i32,
    /// Launched targets are ours to clean up; attached ones are left running.
    launched: bool,
    settled: bool,
    acquired: Instant,
    scratch: String,
    stdout: Option<ChildStdout>,
}

impl AttachedTarget {
    /// Starts `launch` stopped at its `execve`, which is the address space before the dynamic
    /// linker has run: the executable, the interpreter, a stack, and nothing else.
    pub fn launch(launch: &Launch) -> Result<Self> {
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args);
        if let Some(cwd) = &launch.cwd {
            cmd.current_dir(cwd);
        }
        if launch.piped_output {
            cmd.stdout(Stdio::piped());
        }

        let no_aslr = launch.disable_aslr;
        unsafe {
            cmd.pre_exec(move || {
                if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if no_aslr {
                    let current = libc::personality(0xffff_ffff);
                    if current != -1 {
                        libc::personality(
                            current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong,
                        );
                    }
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        let stdout = child.stdout.take();

        match wait_stop(pid)? {
            Stop::Stopped(_) => {}
            other => return Err(gone(pid, other)),
        }
        // If Traveler dies, the target does not sit stopped forever waiting for a tracer that is
        // never coming back.
        set_options(pid)?;

        let mut target = Self::new(pid, stdout);
        target.launched = true;
        Ok(target)
    }

    pub fn attach(pid: u32) -> Result<Self> {
        let pid = pid as i32;
        if unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid, 0, 0) } == -1 {
            return Err(attach_error(pid, io::Error::last_os_error()));
        }
        match wait_stop(pid)? {
            Stop::Stopped(_) => {}
            other => return Err(gone(pid, other)),
        }
        set_options(pid)?;
        Ok(Self::new(pid, None))
    }

    fn new(pid: i32, stdout: Option<ChildStdout>) -> Self {
        Self {
            pid,
            name: read_trimmed(&format!("/proc/{pid}/comm")).unwrap_or_default(),
            helpers: Vec::new(),
            stopped: true,
            pending: 0,
            launched: false,
            settled: true,
            acquired: Instant::now(),
            scratch: String::with_capacity(1 << 16),
            stdout,
        }
    }

    #[inline]
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn is_alive(&self) -> bool {
        std::path::Path::new(&format!("/proc/{}", self.pid)).exists()
    }

    /// Reads the address space. Whether the target is running afterwards is exactly whether it
    /// was running before: a capture observes, it does not decide.
    pub fn capture(&mut self) -> Result<AddressSpace> {
        let was_running = !self.stopped;
        let started = Instant::now();

        self.freeze()?;
        // Nothing is parsed while the target waits. Slurp, resume, then work on bytes we own.
        let maps_path = format!("/proc/{}/maps", self.pid);
        let mut text = std::mem::take(&mut self.scratch);
        let read = slurp(&maps_path, &mut text);
        let threads = 1 + self.helpers.len();
        let settled = self.settled;

        self.release_helpers();
        if was_running {
            self.thaw()?;
        }
        let stop = started.elapsed();
        read?;

        let mappings = maps::parse(&text);
        let space = AddressSpace::single(
            &format!("{} [{}]", self.name, self.pid),
            &mappings,
            Caps(Caps::BACKING_PATH),
            CaptureMeta {
                pid: self.pid as u32,
                wall: SystemTime::now(),
                monotonic: self.acquired.elapsed(),
                stop,
                threads,
                consistent: settled,
            },
        );
        drop(mappings);
        self.scratch = text;
        Ok(space)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.freeze()
    }

    pub fn resume(&mut self) -> Result<()> {
        self.release_helpers();
        self.thaw()
    }

    pub fn read_mem(&self, addr: u64, buf: &mut [u8]) -> Result<usize> {
        let local = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let remote = libc::iovec {
            iov_base: addr as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let n = unsafe { libc::process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
        if n < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(n as usize)
    }

    /// Ptrace is per-thread: seizing the leader stops one thread while the others keep mutating
    /// memory underneath the read. Every capture enumerates the task list and seizes each id,
    /// then enumerates again, because a thread can be created between the two.
    fn freeze(&mut self) -> Result<()> {
        if !self.stopped {
            // Thread-directed, not `kill`: a process-directed SIGSTOP lands on whichever thread
            // happens to take it, and then `waitpid` on the leader waits forever.
            if !tgkill(self.pid, self.pid, libc::SIGSTOP) {
                return Err(Error::Gone(self.pid as u32));
            }
            match wait_stop(self.pid)? {
                Stop::Stopped(libc::SIGSTOP) => self.pending = 0,
                Stop::Stopped(signal) => self.pending = signal,
                other => return Err(gone(self.pid, other)),
            }
            self.stopped = true;
        }

        self.settled = false;
        for _ in 0..MAX_SETTLE_ROUNDS {
            let before = self.helpers.len();
            for tid in task_ids(self.pid)? {
                if tid == self.pid || self.helpers.contains(&tid) {
                    continue;
                }
                // A thread that exits between the listing and the seize is not an error, it is
                // one fewer thread to stop.
                if seize_thread(tid) {
                    self.helpers.push(tid);
                }
            }
            if self.helpers.len() == before {
                let live = task_ids(self.pid)?;
                if live
                    .iter()
                    .all(|t| *t == self.pid || self.helpers.contains(t))
                {
                    self.settled = true;
                    break;
                }
            }
        }
        Ok(())
    }

    fn release_helpers(&mut self) {
        for tid in self.helpers.drain(..) {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, tid, 0, 0) };
        }
    }

    fn thaw(&mut self) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }
        let signal = std::mem::take(&mut self.pending);
        if unsafe { libc::ptrace(libc::PTRACE_CONT, self.pid, 0, signal as libc::c_long) } == -1 {
            return Err(Error::Ptrace {
                call: "PTRACE_CONT",
                pid: self.pid,
                source: io::Error::last_os_error(),
            });
        }
        self.stopped = false;
        Ok(())
    }
}

impl Drop for AttachedTarget {
    fn drop(&mut self) {
        self.release_helpers();
        if !self.launched {
            unsafe { libc::ptrace(libc::PTRACE_DETACH, self.pid, 0, 0) };
            return;
        }
        // A target we started is ours to end, and it has to be reaped or it lingers as a zombie
        // on a parent that is still running.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        let mut status = 0;
        while unsafe { libc::waitpid(self.pid, &mut status, libc::__WALL) } == -1 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}

enum Stop {
    Stopped(i32),
    Exited(i32),
    Killed(i32),
}

/// `__WALL` because a thread of a target is not a child in the `waitpid` sense unless asked for.
fn wait_stop(pid: i32) -> Result<Stop> {
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::__WALL) };
        if r == -1 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Ptrace {
                call: "waitpid",
                pid,
                source: e,
            });
        }
        if libc::WIFSTOPPED(status) {
            return Ok(Stop::Stopped(libc::WSTOPSIG(status)));
        }
        if libc::WIFEXITED(status) {
            return Ok(Stop::Exited(libc::WEXITSTATUS(status)));
        }
        if libc::WIFSIGNALED(status) {
            return Ok(Stop::Killed(libc::WTERMSIG(status)));
        }
    }
}

fn tgkill(tgid: i32, tid: i32, signal: i32) -> bool {
    unsafe { libc::syscall(libc::SYS_tgkill, tgid, tid, signal) == 0 }
}

fn seize_thread(tid: i32) -> bool {
    if unsafe { libc::ptrace(libc::PTRACE_ATTACH, tid, 0, 0) } == -1 {
        return false;
    }
    matches!(wait_stop(tid), Ok(Stop::Stopped(_)))
}

fn set_options(pid: i32) -> Result<()> {
    let options = libc::PTRACE_O_EXITKILL;
    if unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, options) } == -1 {
        return Err(Error::Ptrace {
            call: "PTRACE_SETOPTIONS",
            pid,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn task_ids(pid: i32) -> Result<Vec<i32>> {
    let mut out = Vec::new();
    let dir = match std::fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(dir) => dir,
        Err(_) => return Err(Error::Gone(pid as u32)),
    };
    for entry in dir.flatten() {
        if let Some(tid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            out.push(tid);
        }
    }
    Ok(out)
}

fn slurp(path: &str, buf: &mut String) -> Result<()> {
    buf.clear();
    File::open(path)?.read_to_string(buf)?;
    Ok(())
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn gone(pid: i32, stop: Stop) -> Error {
    match stop {
        Stop::Stopped(_) => Error::Gone(pid as u32),
        Stop::Exited(code) => Error::Denied(format!("process {pid} exited with {code}")),
        Stop::Killed(signal) => {
            Error::Denied(format!("process {pid} was killed by signal {signal}"))
        }
    }
}

/// `ptrace_scope` is the usual reason attaching fails, and the message has to say so or the user
/// is left with a bare "operation not permitted".
fn attach_error(pid: i32, source: io::Error) -> Error {
    if source.kind() != io::ErrorKind::PermissionDenied {
        return Error::Ptrace {
            call: "PTRACE_ATTACH",
            pid,
            source,
        };
    }
    let scope = read_trimmed("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_default();
    let why = match scope.as_str() {
        "1" => " (ptrace_scope is 1: only descendants may be traced, so launch it instead)",
        "2" => " (ptrace_scope is 2: attaching needs CAP_SYS_PTRACE)",
        "3" => " (ptrace_scope is 3: attaching is disabled on this kernel)",
        _ => "",
    };
    Error::Denied(format!("cannot attach to {pid}{why}"))
}
