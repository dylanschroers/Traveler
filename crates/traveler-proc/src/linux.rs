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
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStdout, Command, Stdio};
use std::time::{Instant, SystemTime};

use crate::control::{Breakpoints, Frame, StopReason, TRAP};
use crate::pages::{PageBits, PageLimits, RawPages};
use crate::{AddressSpace, Caps, CaptureMeta, Error, Result, SectionSource, maps};

/// What one capture is allowed to cost the target.
#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureOptions {
    pub pages: PageLimits,
    /// Skip the pagemap read entirely. The mapping level does not need it.
    pub without_pages: bool,
    /// Write `4` to `clear_refs`, so the *next* capture's soft-dirty bits mean "written since
    /// this one". It costs the target: the kernel write-protects its page tables and the program
    /// takes extra faults afterwards, which is why it is opt-in per capture and says so.
    pub clear_soft_dirty: bool,
}

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
    /// Phase 9's patch table. Empty unless something asked for a breakpoint, and nothing in the
    /// capture path reads it.
    breaks: Breakpoints,
    /// A breakpoint the program counter is sitting on, which has to be stepped over before it can
    /// be put back or the target traps on it immediately.
    resting_on: Option<u64>,
    settled: bool,
    acquired: Instant,
    scratch: String,
    /// Kept across captures so a repeated capture does not re-open it inside the stop.
    pagemap: Option<File>,
    /// Opened only when something is actually written, since a poke is rare and the handle is
    /// what allows writing at all.
    mem: Option<File>,
    words: Vec<u64>,
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
            breaks: Breakpoints::default(),
            resting_on: None,
            settled: true,
            acquired: Instant::now(),
            scratch: String::with_capacity(1 << 16),
            pagemap: None,
            mem: None,
            words: Vec::new(),
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

    pub fn capture(&mut self) -> Result<AddressSpace> {
        self.capture_with(&CaptureOptions::default())
    }

    pub fn capture_with(&mut self, options: &CaptureOptions) -> Result<AddressSpace> {
        self.capture_sections(options, &mut |_| Vec::new())
    }

    /// Reads the address space. Whether the target is running afterwards is exactly whether it
    /// was running before: a capture observes, it does not decide.
    ///
    /// `sections` runs after the target is resumed, on mappings this already owns. It is a
    /// callback rather than a dependency because `proc` asks the operating system and `sym` reads
    /// files, and neither should have to know about the other.
    pub fn capture_sections(
        &mut self,
        options: &CaptureOptions,
        sections: SectionSource<'_>,
    ) -> Result<AddressSpace> {
        let was_running = !self.stopped;
        let started = Instant::now();

        self.freeze()?;
        // Inside the window: raw reads and the one scan pagemap cannot do without. Everything
        // else — full parsing, the tree, interning, layout — waits until the target is running.
        let mut text = std::mem::take(&mut self.scratch);
        let read = slurp(&format!("/proc/{}/maps", self.pid), &mut text);
        let raw = match &read {
            Ok(()) if !options.without_pages => self.read_pages(&maps::extents(&text), options),
            _ => RawPages::default(),
        };
        if options.clear_soft_dirty {
            let _ = std::fs::write(format!("/proc/{}/clear_refs", self.pid), b"4");
        }
        let threads = 1 + self.helpers.len();
        let settled = self.settled;

        self.release_helpers();
        if was_running {
            self.thaw()?;
        }
        let stop = started.elapsed();
        read?;

        let pages = PageBits::decode(&raw);
        let mut caps = Caps::BACKING_PATH;
        if !options.without_pages {
            caps |= Caps::PAGE_RESIDENCY | Caps::PAGE_SWAPPED;
            if soft_dirty_works() {
                caps |= Caps::SOFT_DIRTY | Caps::WRITE_TRACKING;
            }
        }

        let mappings = maps::parse(&text);
        let sections = sections(&mappings);
        let space = AddressSpace::single(
            &format!("{} [{}]", self.name, self.pid),
            &mappings,
            &sections,
            pages,
            Caps(caps),
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

    /// One `pread` per chosen mapping, into a buffer reused across captures.
    fn read_pages(&mut self, extents: &[(u64, u64)], options: &CaptureOptions) -> RawPages {
        let mut raw = RawPages::with_capacity(extents.len());
        if self.pagemap.is_none() {
            self.pagemap = File::open(format!("/proc/{}/pagemap", self.pid)).ok();
        }
        let Some(file) = &self.pagemap else {
            return raw;
        };
        let fd = file.as_raw_fd();

        let mut words = std::mem::take(&mut self.words);
        for i in RawPages::plan(extents, &options.pages) {
            let (start, end) = extents[i];
            let count = ((end - start) / crate::pages::PAGE) as usize;
            words.clear();
            words.resize(count, 0);
            if pread_words(fd, start / crate::pages::PAGE * 8, &mut words) {
                raw.set(i, &words);
            }
        }
        self.words = words;
        raw
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

    /// Writes bytes into the target at a stop.
    ///
    /// Deliberately `/proc/pid/mem` rather than `process_vm_writev`, which cannot write a
    /// read-only page. This goes through `FOLL_FORCE` and can, which is how gdb patches `.text`.
    ///
    /// The stop is what makes it safe: no race with the target's threads, no torn write, and
    /// nothing to reason about regarding memory ordering against a running program. As with a
    /// capture, whether the target runs afterwards is exactly whether it ran before.
    pub fn write_mem(&mut self, addr: u64, bytes: &[u8]) -> Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let was_running = !self.stopped;
        self.freeze()?;

        let written = self.write_at(addr, bytes);
        self.release_helpers();
        if was_running {
            self.thaw()?;
        }
        written
    }

    fn write_at(&mut self, addr: u64, bytes: &[u8]) -> Result<usize> {
        if self.mem.is_none() {
            self.mem = Some(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(format!("/proc/{}/mem", self.pid))?,
            );
        }
        let fd = self.mem.as_ref().expect("just opened").as_raw_fd();
        let mut done = 0usize;
        while done < bytes.len() {
            let n = unsafe {
                libc::pwrite(
                    fd,
                    bytes[done..].as_ptr().cast(),
                    bytes.len() - done,
                    (addr + done as u64) as libc::off_t,
                )
            };
            if n <= 0 {
                let e = io::Error::last_os_error();
                return if done > 0 {
                    Ok(done)
                } else {
                    Err(Error::Io(e))
                };
            }
            done += n as usize;
        }
        Ok(done)
    }

    pub fn breakpoints(&self) -> &Breakpoints {
        &self.breaks
    }

    /// Patches one byte to `0xCC`, keeping what was there.
    pub fn set_breakpoint(&mut self, addr: u64) -> Result<()> {
        if self.breaks.at(addr).is_some() {
            return Ok(());
        }
        let was_running = !self.stopped;
        self.freeze()?;

        let mut original = [0u8; 1];
        let result = self
            .read_mem(addr, &mut original)
            .and_then(|n| match n {
                1 => self.write_at(addr, &[TRAP]).map(|_| original[0]),
                _ => Err(Error::Io(io::Error::from(io::ErrorKind::InvalidInput))),
            })
            .inspect(|byte| self.breaks.insert(addr, *byte));

        self.release_helpers();
        if was_running {
            self.thaw()?;
        }
        result.map(|_| ())
    }

    pub fn clear_breakpoint(&mut self, addr: u64) -> Result<()> {
        let Some(found) = self.breaks.take(addr) else {
            return Ok(());
        };
        let was_running = !self.stopped;
        self.freeze()?;
        let result = self.write_at(addr, &[found.original]);
        self.release_helpers();
        if was_running {
            self.thaw()?;
        }
        result.map(|_| ())
    }

    /// Resumes and waits for whatever stops it next.
    ///
    /// The dance around a breakpoint the program counter is already on is the whole reason this
    /// cannot be `PTRACE_CONT` alone: the trap has to come out, one instruction has to run, and
    /// the trap has to go back, or the target either traps forever or misses it on every later
    /// pass.
    pub fn resume_until(&mut self) -> Result<StopReason> {
        // A running target cannot be continued: `PTRACE_CONT` needs a ptrace-stop, and asking a
        // moving process to resume is `ESRCH`. Stopping first is invisible and is what a debugger
        // does anyway.
        self.freeze()?;
        self.release_helpers();

        if let Some(addr) = self.resting_on.take()
            && let Some(found) = self.breaks.at(addr).copied()
        {
            self.write_at(addr, &[found.original])?;
            self.step_raw()?;
            self.write_at(addr, &[TRAP])?;
        }
        self.stopped = false;
        let signal = std::mem::take(&mut self.pending);
        self.cont(libc::PTRACE_CONT, signal)?;
        self.wait_reason()
    }

    /// One instruction.
    pub fn step(&mut self) -> Result<StopReason> {
        self.freeze()?;
        self.release_helpers();
        if let Some(addr) = self.resting_on.take()
            && let Some(found) = self.breaks.at(addr).copied()
        {
            self.write_at(addr, &[found.original])?;
            let reason = self.step_raw();
            self.write_at(addr, &[TRAP])?;
            reason?;
            return Ok(StopReason::Step);
        }
        self.step_raw()?;
        Ok(StopReason::Step)
    }

    fn step_raw(&mut self) -> Result<()> {
        self.cont(libc::PTRACE_SINGLESTEP, 0)?;
        match wait_stop(self.pid)? {
            Stop::Stopped(_) => Ok(()),
            other => Err(gone(self.pid, other)),
        }
    }

    fn cont(&mut self, request: libc::c_uint, signal: i32) -> Result<()> {
        if unsafe { libc::ptrace(request, self.pid, 0, signal as libc::c_long) } == -1 {
            return Err(Error::Ptrace {
                call: "PTRACE_CONT",
                pid: self.pid,
                source: io::Error::last_os_error(),
            });
        }
        Ok(())
    }

    /// Classifies a stop, and rewinds the program counter off a trap it just executed.
    fn wait_reason(&mut self) -> Result<StopReason> {
        let reason = match wait_stop(self.pid)? {
            Stop::Exited(code) => return Ok(StopReason::Exited(code)),
            Stop::Killed(signal) => return Ok(StopReason::Killed(signal)),
            Stop::Stopped(libc::SIGTRAP) => {
                self.stopped = true;
                let pc = self.program_counter()?;
                // The trap has already run, so the counter is one past it.
                match self.breaks.at(pc.wrapping_sub(1)).map(|b| b.addr) {
                    Some(addr) => {
                        self.set_program_counter(addr)?;
                        self.resting_on = Some(addr);
                        StopReason::Breakpoint(addr)
                    }
                    None => StopReason::Step,
                }
            }
            Stop::Stopped(signal) => {
                self.stopped = true;
                self.pending = signal;
                StopReason::Signal(signal)
            }
        };
        Ok(reason)
    }

    pub fn program_counter(&self) -> Result<u64> {
        Ok(self.registers()?.rip)
    }

    fn set_program_counter(&mut self, pc: u64) -> Result<()> {
        let mut regs = self.registers()?;
        regs.rip = pc;
        let mut iov = libc::iovec {
            iov_base: (&raw mut regs).cast(),
            iov_len: size_of::<libc::user_regs_struct>(),
        };
        if unsafe {
            libc::ptrace(
                libc::PTRACE_SETREGSET,
                self.pid,
                libc::NT_PRSTATUS,
                &mut iov,
            )
        } == -1
        {
            return Err(Error::Ptrace {
                call: "PTRACE_SETREGSET",
                pid: self.pid,
                source: io::Error::last_os_error(),
            });
        }
        Ok(())
    }

    pub fn registers(&self) -> Result<libc::user_regs_struct> {
        let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec {
            iov_base: (&raw mut regs).cast(),
            iov_len: size_of::<libc::user_regs_struct>(),
        };
        if unsafe {
            libc::ptrace(
                libc::PTRACE_GETREGSET,
                self.pid,
                libc::NT_PRSTATUS,
                &mut iov,
            )
        } == -1
        {
            return Err(Error::Ptrace {
                call: "PTRACE_GETREGSET",
                pid: self.pid,
                source: io::Error::last_os_error(),
            });
        }
        Ok(regs)
    }

    /// Walks the frame-pointer chain.
    ///
    /// Honest about what it is: this needs frame pointers, and code built without them unwinds to
    /// one frame and stops. Doing better means reading `.eh_frame` CFI, which is a real subsystem
    /// and not one this phase claims to have.
    pub fn backtrace(&self, limit: usize) -> Result<Vec<Frame>> {
        let regs = self.registers()?;
        let mut out = vec![Frame {
            pc: regs.rip,
            frame: regs.rbp,
        }];
        let mut frame = regs.rbp;
        for _ in 1..limit {
            if frame == 0 || frame % 8 != 0 {
                break;
            }
            let mut pair = [0u8; 16];
            if self.read_mem(frame, &mut pair).unwrap_or(0) != 16 {
                break;
            }
            let next = u64::from_ne_bytes(pair[..8].try_into().expect("eight bytes"));
            let pc = u64::from_ne_bytes(pair[8..].try_into().expect("eight bytes"));
            if pc == 0 || next <= frame {
                break;
            }
            out.push(Frame { pc, frame: next });
            frame = next;
        }
        Ok(out)
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

/// `pread` until the buffer is full. A short read means the rest of the range has no page table,
/// which is a fact about the target rather than an error.
fn pread_words(fd: i32, offset: u64, words: &mut [u64]) -> bool {
    let bytes = std::mem::size_of_val(words);
    let base = words.as_mut_ptr().cast::<u8>();
    let mut done = 0usize;
    while done < bytes {
        let n = unsafe {
            libc::pread(
                fd,
                base.add(done).cast(),
                bytes - done,
                (offset + done as u64) as libc::off_t,
            )
        };
        if n <= 0 {
            return done == bytes;
        }
        done += n as usize;
    }
    true
}

/// Soft-dirty needs `CONFIG_MEM_SOFT_DIRTY`, and a kernel without it accepts the `clear_refs`
/// write and then reports bit 55 as zero forever. The only honest test is to try it on ourselves:
/// clear, write a page, and see whether the bit came back.
fn soft_dirty_works() -> bool {
    use std::sync::OnceLock;
    static ANSWER: OnceLock<bool> = OnceLock::new();
    *ANSWER.get_or_init(|| {
        let mut probe = vec![0u8; crate::pages::PAGE as usize * 2];
        let page = (probe.as_mut_ptr() as u64).next_multiple_of(crate::pages::PAGE);
        unsafe { std::ptr::write_volatile(page as *mut u64, 1) };
        if std::fs::write("/proc/self/clear_refs", b"4").is_err() {
            return false;
        }
        unsafe { std::ptr::write_volatile(page as *mut u64, 2) };
        let Ok(file) = File::open("/proc/self/pagemap") else {
            return false;
        };
        let mut word = [0u64; 1];
        if !pread_words(file.as_raw_fd(), page / crate::pages::PAGE * 8, &mut word) {
            return false;
        }
        word[0] & (1 << 55) != 0
    })
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
