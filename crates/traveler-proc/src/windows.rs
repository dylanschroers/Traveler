//! Acquire and capture, Windows.
//!
//! Written against the snapshot shape 6c settled and before anything is built on top of it, which
//! is what "abstracted from day one" was supposed to mean: the trait and the struct were designed
//! against both capability lists, not written for Linux and then bent.
//!
//! The plumbing differs everywhere and the shape does not. Windows hands over the region level for
//! free through `AllocationBase`, where Linux has to infer it; Windows has no per-page dirty bit
//! for another process, where Linux has soft-dirty. Both facts are capability flags rather than a
//! lowest common denominator.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Instant, SystemTime};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    GetMappedFileNameW, PSAPI_WORKING_SET_EX_INFORMATION, QueryWorkingSetEx,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, GetExitCodeProcess, OpenProcess, OpenThread, PROCESS_QUERY_INFORMATION,
    PROCESS_SUSPEND_RESUME, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, ResumeThread,
    SuspendThread, THREAD_SUSPEND_RESUME, TerminateProcess,
};

use crate::pages::{PAGE, PageBits, PageLimits, RawPages};
use crate::{AddressSpace, Caps, CaptureMeta, Error, MappingFacts, Prot, Result, SectionSource};

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureOptions {
    pub pages: PageLimits,
    pub without_pages: bool,
    /// Accepted and ignored. `GetWriteWatch` only works on memory the target allocated itself with
    /// `MEM_WRITE_WATCH`, so there is no per-page dirty bit for an arbitrary process. The
    /// capability flag says so rather than the view quietly showing zeros.
    pub clear_soft_dirty: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// No effect here. Windows randomises per image at link time, and there is no per-process
    /// switch a launcher can set, so two runs are comparable only if the image opted out.
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

pub struct AttachedTarget {
    pid: u32,
    process: HANDLE,
    name: String,
    /// Kept so the handle stays alive and so a launched target can be terminated on drop.
    child: Option<Child>,
    stopped: bool,
    acquired: Instant,
    working_set: Vec<PSAPI_WORKING_SET_EX_INFORMATION>,
    stdout: Option<ChildStdout>,
}

// The handle is owned by this struct and every use is behind `&self` or `&mut self`. Unlike
// ptrace, Windows has no rule that requests must come from the thread that attached.
unsafe impl Send for AttachedTarget {}
unsafe impl Sync for AttachedTarget {}

const ACCESS: u32 = PROCESS_QUERY_INFORMATION
    | PROCESS_VM_READ
    | PROCESS_VM_WRITE
    | PROCESS_VM_OPERATION
    | PROCESS_SUSPEND_RESUME;

impl AttachedTarget {
    /// `CREATE_SUSPENDED` is the analogue of the `execve` stop: the image and `ntdll` are mapped
    /// and the main thread has not run, so capture 0 is the address space before the loader.
    pub fn launch(launch: &Launch) -> Result<Self> {
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args);
        if let Some(cwd) = &launch.cwd {
            cmd.current_dir(cwd);
        }
        if launch.piped_output {
            cmd.stdout(Stdio::piped());
        }
        cmd.creation_flags(CREATE_SUSPENDED);

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let process = child.as_raw_handle() as HANDLE;
        let name = PathBuf::from(&launch.program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        Ok(Self {
            pid,
            process,
            name,
            child: Some(child),
            stopped: true,
            acquired: Instant::now(),
            working_set: Vec::new(),
            stdout,
        })
    }

    pub fn attach(pid: u32) -> Result<Self> {
        let process = unsafe { OpenProcess(ACCESS, 0, pid) };
        if process.is_null() {
            let source = io::Error::last_os_error();
            return Err(match source.kind() {
                io::ErrorKind::PermissionDenied => Error::Denied(format!(
                    "cannot open {pid} (a process at a higher integrity level needs an elevated \
                     Traveler, and a protected one cannot be opened at all)"
                )),
                _ => Error::Ptrace {
                    call: "OpenProcess",
                    pid: pid as i32,
                    source,
                },
            });
        }
        let mut target = Self {
            pid,
            process,
            name: String::new(),
            child: None,
            stopped: false,
            acquired: Instant::now(),
            working_set: Vec::new(),
            stdout: None,
        };
        target.freeze()?;
        Ok(target)
    }

    #[inline]
    pub fn pid(&self) -> u32 {
        self.pid
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
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code) != 0 && code == STILL_ACTIVE as u32 }
    }

    pub fn capture(&mut self) -> Result<AddressSpace> {
        self.capture_with(&CaptureOptions::default())
    }

    pub fn capture_with(&mut self, options: &CaptureOptions) -> Result<AddressSpace> {
        self.capture_sections(options, &mut |_| Vec::new())
    }

    /// Same policy as Linux: a capture observes, it does not decide whether the target runs.
    pub fn capture_sections(
        &mut self,
        options: &CaptureOptions,
        sections: SectionSource<'_>,
    ) -> Result<AddressSpace> {
        let was_running = !self.stopped;
        let started = Instant::now();

        self.freeze()?;
        let regions = self.query_regions();
        let raw = if options.without_pages {
            RawPages::default()
        } else {
            let extents: Vec<(u64, u64)> =
                regions.iter().map(|r| (r.addr, r.addr + r.len)).collect();
            self.read_working_set(&extents, options)
        };
        let threads = self.thread_ids().len();

        if was_running {
            self.thaw()?;
        }
        let stop = started.elapsed();

        let paths: Vec<String> = regions.iter().map(|r| self.mapped_file(r.addr)).collect();
        let mappings: Vec<MappingFacts<'_>> = regions
            .iter()
            .zip(&paths)
            .map(|(r, path)| MappingFacts {
                addr: r.addr,
                len: r.len,
                prot: r.prot,
                path,
                offset: 0,
                dev: 0,
                inode: 0,
                // The whole point of the Windows backend: the region level is recorded rather
                // than inferred from adjacency and backing.
                group: r.allocation_base,
            })
            .collect();

        // No PAGE_SWAPPED, no SOFT_DIRTY, no WRITE_TRACKING. `GetWriteWatch` only works on
        // memory the target allocated itself with `MEM_WRITE_WATCH`, so there is no per-page dirty
        // bit for an arbitrary process, and §17 says degrade honestly rather than paper over it.
        let mut caps = Caps::BACKING_PATH;
        if !options.without_pages {
            caps |= Caps::PAGE_RESIDENCY;
        }

        let sections = sections(&mappings);
        Ok(AddressSpace::single(
            &format!("{} [{}]", self.name, self.pid),
            &mappings,
            &sections,
            PageBits::decode(&raw),
            Caps(caps),
            CaptureMeta {
                pid: self.pid,
                wall: SystemTime::now(),
                monotonic: self.acquired.elapsed(),
                stop,
                threads,
                // Every thread is suspended for the whole read, and unlike ptrace there is no
                // per-thread attach to race with.
                consistent: true,
            },
        ))
    }

    pub fn stop(&mut self) -> Result<()> {
        self.freeze()
    }

    pub fn resume(&mut self) -> Result<()> {
        self.thaw()
    }

    pub fn read_mem(&self, addr: u64, buf: &mut [u8]) -> Result<usize> {
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                self.process,
                addr as *const _,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut read,
            )
        };
        if ok == 0 && read == 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(read)
    }

    /// Suspend every thread from a Toolhelp32 snapshot. The same requirement as Linux with
    /// different plumbing: a snapshot taken while other threads run is a torn one.
    fn freeze(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        for tid in self.thread_ids() {
            with_thread(tid, |handle| unsafe {
                SuspendThread(handle);
            });
        }
        self.stopped = true;
        Ok(())
    }

    fn thaw(&mut self) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }
        for tid in self.thread_ids() {
            with_thread(tid, |handle| unsafe {
                ResumeThread(handle);
            });
        }
        self.stopped = false;
        Ok(())
    }

    fn thread_ids(&self) -> Vec<u32> {
        let mut out = Vec::new();
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
        while ok != 0 {
            if entry.th32OwnerProcessID == self.pid {
                out.push(entry.th32ThreadID);
            }
            ok = unsafe { Thread32Next(snapshot, &mut entry) };
        }
        unsafe { CloseHandle(snapshot) };
        out
    }

    /// Walk the address space by `BaseAddress + RegionSize`, skipping free ranges: a 64-bit
    /// address space is essentially all hole, and §2 collapses unmapped VA to nothing.
    fn query_regions(&self) -> Vec<Region> {
        let mut out = Vec::new();
        let mut addr: u64 = 0;
        loop {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let n = unsafe {
                VirtualQueryEx(
                    self.process,
                    addr as *const _,
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if n == 0 {
                break;
            }
            let base = info.BaseAddress as u64;
            let size = info.RegionSize as u64;
            if size == 0 {
                break;
            }
            if info.State != MEM_FREE {
                out.push(Region {
                    addr: base,
                    len: size,
                    allocation_base: info.AllocationBase as u64,
                    prot: protection(&info),
                });
            }
            match base.checked_add(size) {
                Some(next) => addr = next,
                None => break,
            }
        }
        out
    }

    /// `QueryWorkingSetEx` is the `pagemap` analogue, and it is priced the same way: one entry per
    /// page, so the caps in §7 apply unchanged.
    fn read_working_set(&mut self, extents: &[(u64, u64)], options: &CaptureOptions) -> RawPages {
        let mut raw = RawPages::with_capacity(extents.len());
        let mut words: Vec<u64> = Vec::new();
        let mut buffer = std::mem::take(&mut self.working_set);

        for i in RawPages::plan(extents, &options.pages) {
            let (start, end) = extents[i];
            let count = ((end - start) / PAGE) as usize;
            buffer.clear();
            buffer.reserve(count);
            for page in 0..count {
                buffer.push(PSAPI_WORKING_SET_EX_INFORMATION {
                    VirtualAddress: (start + page as u64 * PAGE) as *mut _,
                    VirtualAttributes: unsafe { std::mem::zeroed() },
                });
            }
            let bytes = std::mem::size_of_val(buffer.as_slice()) as u32;
            let ok = unsafe { QueryWorkingSetEx(self.process, buffer.as_mut_ptr().cast(), bytes) };
            if ok == 0 {
                continue;
            }
            words.clear();
            words.extend(buffer.iter().map(|entry| {
                // Re-expressed in pagemap's bit positions so one decoder serves both backends.
                // Bit 0 of the block is Valid; there is no swapped bit and no dirty bit here,
                // which is what the capability flags already say.
                let flags = unsafe { entry.VirtualAttributes.Flags };
                if flags & 1 != 0 { 1u64 << 63 } else { 0 }
            }));
            raw.set(i, &words);
        }

        self.working_set = buffer;
        raw
    }

    /// `GetMappedFileNameW` answers in device form, `\Device\HarddiskVolume3\...`, which is not a
    /// path anyone can read. Converting it needs the DOS drive table.
    fn mapped_file(&self, addr: u64) -> String {
        let mut buf = [0u16; 512];
        let n = unsafe {
            GetMappedFileNameW(
                self.process,
                addr as *mut _,
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        };
        if n == 0 {
            return String::new();
        }
        let device = OsString::from_wide(&buf[..n as usize])
            .to_string_lossy()
            .into_owned();
        dos_path(&device)
    }
}

impl Drop for AttachedTarget {
    fn drop(&mut self) {
        if self.child.is_some() {
            // A target we started is ours to end. `Child` owns the handle, so it closes itself.
            unsafe { TerminateProcess(self.process, 1) };
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
            return;
        }
        let _ = self.thaw();
        unsafe { CloseHandle(self.process) };
    }
}

struct Region {
    addr: u64,
    len: u64,
    allocation_base: u64,
    prot: Prot,
}

fn protection(info: &MEMORY_BASIC_INFORMATION) -> Prot {
    // A reserved-but-uncommitted range is mapped address space with no access, which is exactly
    // what a `PROT_NONE` guard is on the other side.
    if info.State != MEM_COMMIT {
        return Prot::default();
    }
    let mut bits = 0;
    let protect = info.Protect & 0xff;
    if matches!(
        protect,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    ) {
        bits |= Prot::READ;
    }
    if matches!(
        protect,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    ) {
        bits |= Prot::WRITE;
    }
    if matches!(
        protect,
        PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    ) {
        bits |= Prot::EXEC;
    }
    if info.Type == MEM_IMAGE || info.Type == MEM_MAPPED {
        bits |= Prot::SHARED;
    }
    Prot(bits)
}

fn with_thread(tid: u32, f: impl FnOnce(HANDLE)) {
    let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, tid) };
    if handle.is_null() {
        return;
    }
    f(handle);
    unsafe { CloseHandle(handle) };
}

/// `\Device\HarddiskVolume3\x` back to `C:\x`, by asking each drive letter what device it is.
fn dos_path(device: &str) -> String {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;

    let mut name = [0u16; 8];
    let mut target = [0u16; 512];
    for letter in b'A'..=b'Z' {
        name[0] = letter as u16;
        name[1] = b':' as u16;
        name[2] = 0;
        let n = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
        if n == 0 {
            continue;
        }
        let mapped = OsString::from_wide(&target[..n as usize])
            .to_string_lossy()
            .trim_end_matches('\0')
            .to_string();
        if let Some(rest) = device.strip_prefix(mapped.as_str()) {
            return format!("{}:{}", letter as char, rest);
        }
    }
    device.to_string()
}
