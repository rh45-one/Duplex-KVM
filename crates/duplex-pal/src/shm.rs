//! Cross-Platform Shared Memory (`ShmWatchdog`) & Monotonic Clock Primitives.
//!
//! Uses `shm_open` + `ftruncate` + `mmap` on Debian Linux and
//! `CreateFileMappingW` + `MapViewOfFile` on Windows 11.

use crate::PalError;
use core::ptr::NonNull;
use duplex_proto::ShmWatchdogBlock;

/// Returns a monotonic timestamp in nanoseconds (`CLOCK_MONOTONIC` on Linux, `QueryPerformanceCounter` on Windows).
#[inline]
pub fn monotonic_now_ns() -> u64 {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    }

    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };
        let mut count = 0i64;
        let mut freq = 0i64;
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
        if freq <= 0 {
            return 0;
        }
        ((count as u128).saturating_mul(1_000_000_000u128) / (freq as u128)) as u64
    }
}

/// Checks whether the OS process `pid` is still alive.
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let rc = libc::kill(pid as libc::pid_t, 0);
        if rc == 0 {
            // An exited, unreaped child still answers kill(pid, 0). It must not
            // be treated as a live daemon or prevent the next owner starting.
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if let Some((_, fields)) = stat.rsplit_once(')') {
                    if matches!(fields.split_whitespace().next(), Some("Z" | "X")) {
                        return false;
                    }
                }
            }
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut exit_code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut exit_code).is_ok();
            let _ = CloseHandle(handle);
            ok && exit_code == (STILL_ACTIVE.0 as u32)
        } else {
            false
        }
    }
}

/// Sends an emergency termination signal (`SIGKILL` / `TerminateProcess`) to a hung `duplexd` process
/// so the OS kernel immediately closes any held `evdev` fds or Win32 low-level hooks.
pub fn force_terminate_process(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL) == 0
    }

    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let ok = TerminateProcess(handle, 1).is_ok();
            let _ = CloseHandle(handle);
            ok
        } else {
            false
        }
    }
}

#[cfg(target_os = "windows")]
struct DaemonMutex(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for DaemonMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Cross-platform shared memory region mapping `ShmWatchdogBlock`.
pub struct ShmWatchdog {
    ptr: NonNull<ShmWatchdogBlock>,
    #[cfg(target_os = "linux")]
    fd: libc::c_int,
    #[cfg(target_os = "windows")]
    handle: windows::Win32::Foundation::HANDLE,
    is_owner: bool,
    #[cfg(target_os = "windows")]
    _daemon_mutex: Option<DaemonMutex>,
}

// SAFETY: `ShmWatchdogBlock` contains exclusively `Atomic*` fields and is synchronized across processes/threads.
unsafe impl Send for ShmWatchdog {}
unsafe impl Sync for ShmWatchdog {}

impl ShmWatchdog {
    /// Creates or opens the shared-memory watchdog block.
    /// When `create_and_init` is `true` (`duplexd`), initializes the block with the current PID.
    pub fn open(create_and_init: bool) -> Result<Self, PalError> {
        #[cfg(target_os = "linux")]
        {
            Self::open_linux(create_and_init)
        }
        #[cfg(target_os = "windows")]
        {
            Self::open_windows(create_and_init)
        }
    }

    #[inline]
    pub fn block(&self) -> &ShmWatchdogBlock {
        unsafe { self.ptr.as_ref() }
    }

    /// Process liveness alone is insufficient during kernel device teardown:
    /// an exiting leader can be a zombie before its last file handle closes.
    /// Reader mappings can probe the ownership lock without resetting telemetry.
    pub fn daemon_ownership_available(&self) -> bool {
        if self.is_owner {
            return false;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            if libc::flock(self.fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
                return false;
            }
            libc::flock(self.fd, libc::LOCK_UN) == 0
        }
        #[cfg(target_os = "windows")]
        {
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
            use windows::Win32::System::Threading::CreateMutexW;
            let name: Vec<u16> = format!("{}-daemon-owner", duplex_proto::WINDOWS_SHM_NAME)
                .encode_utf16()
                .chain(Some(0))
                .collect();
            match unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) } {
                Ok(handle) => {
                    let available = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
                    drop(DaemonMutex(handle));
                    available
                }
                Err(_) => false,
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn open_linux(create_and_init: bool) -> Result<Self, PalError> {
        Self::open_linux_named(create_and_init, duplex_proto::LINUX_SHM_NAME)
    }

    #[cfg(target_os = "linux")]
    fn open_linux_named(create_and_init: bool, name: &str) -> Result<Self, PalError> {
        use std::ffi::CString;

        let c_name = CString::new(name).unwrap();
        let flags = if create_and_init {
            libc::O_CREAT | libc::O_RDWR
        } else {
            libc::O_RDWR
        };

        let fd = unsafe { libc::shm_open(c_name.as_ptr(), flags, 0o666) };
        if fd < 0 {
            return Err(PalError::SharedMemory(format!(
                "shm_open({}) failed: {}",
                name,
                std::io::Error::last_os_error()
            )));
        }

        // Lock the same kernel object that stores the daemon identity. Readers
        // do not lock it; only a new daemon must acquire exclusive ownership.
        if create_and_init && unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(PalError::SharedMemory(format!(
                "Cannot acquire daemon ownership; another Duplex daemon may already be running: {error}")));
        }

        let size = core::mem::size_of::<ShmWatchdogBlock>();
        if create_and_init {
            unsafe {
                let _ = libc::fchmod(fd, 0o666);
            }
            let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
            if rc != 0 {
                unsafe { libc::close(fd) };
                return Err(PalError::SharedMemory(format!(
                    "ftruncate failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

        let map_ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if map_ptr == libc::MAP_FAILED || map_ptr.is_null() {
            unsafe { libc::close(fd) };
            return Err(PalError::SharedMemory(format!(
                "mmap failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let ptr = NonNull::new(map_ptr as *mut ShmWatchdogBlock).unwrap();
        let shm = Self {
            ptr,
            fd,
            is_owner: create_and_init,
        };

        if create_and_init {
            shm.initialize_owner()?;
        }

        Ok(shm)
    }

    #[cfg(target_os = "windows")]
    fn open_windows(create_and_init: bool) -> Result<Self, PalError> {
        Self::open_windows_named(create_and_init, duplex_proto::WINDOWS_SHM_NAME)
    }

    #[cfg(target_os = "windows")]
    fn open_windows_named(create_and_init: bool, name: &str) -> Result<Self, PalError> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
        };

        let wide_name: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();
        let size = core::mem::size_of::<ShmWatchdogBlock>() as u32;

        let daemon_mutex = if create_and_init {
            use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
            use windows::Win32::System::Threading::CreateMutexW;
            let mutex_name: Vec<u16> = format!("{name}-daemon-owner")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let handle = unsafe { CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) }
                .map_err(|e| {
                    PalError::SharedMemory(format!("Cannot acquire daemon ownership: {e}"))
                })?;
            let mutex = DaemonMutex(handle);
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                return Err(PalError::SharedMemory("Another Duplex daemon is already running; stop it before starting a new daemon".into()));
            }
            Some(mutex)
        } else {
            None
        };

        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                size,
                PCWSTR(wide_name.as_ptr()),
            )
        }
        .map_err(|e| PalError::SharedMemory(format!("CreateFileMappingW failed: {e}")))?;

        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size as usize) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(PalError::SharedMemory(
                "MapViewOfFile returned null pointer".into(),
            ));
        }

        let ptr = NonNull::new(view.Value as *mut ShmWatchdogBlock).unwrap();
        let shm = Self {
            ptr,
            handle,
            is_owner: create_and_init,
            _daemon_mutex: daemon_mutex,
        };

        if create_and_init {
            shm.initialize_owner()?;
        }

        Ok(shm)
    }
    fn initialize_owner(&self) -> Result<(), PalError> {
        if self.block().is_valid() {
            let previous = self
                .block()
                .daemon_pid
                .load(core::sync::atomic::Ordering::Acquire);
            if previous != std::process::id() && is_process_alive(previous) {
                return Err(PalError::SharedMemory(format!(
                    "Another Duplex daemon (PID {previous}) is still running. Stop the older daemon before starting this version.")));
            }
        }
        self.block()
            .init_daemon(std::process::id(), monotonic_now_ns());
        Ok(())
    }
}

impl Drop for ShmWatchdog {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            let size = core::mem::size_of::<ShmWatchdogBlock>();
            let _ = libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, size);
            let _ = libc::close(self.fd);
            let _ = self.is_owner; // Keep SHM object persistent for duplex-guard inspection
        }

        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr.as_ptr() as *mut core::ffi::c_void,
            });
            let _ = CloseHandle(self.handle);
            let _ = self.is_owner;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    fn unique_name(label: &str) -> String {
        #[cfg(target_os = "linux")]
        let prefix = "/";
        #[cfg(target_os = "windows")]
        let prefix = "Local\\";
        format!(
            "{prefix}duplex-owner-test-{}-{}-{label}",
            std::process::id(),
            monotonic_now_ns()
        )
    }

    fn open_named(init: bool, name: &str) -> Result<ShmWatchdog, PalError> {
        #[cfg(target_os = "linux")]
        {
            ShmWatchdog::open_linux_named(init, name)
        }
        #[cfg(target_os = "windows")]
        {
            ShmWatchdog::open_windows_named(init, name)
        }
    }

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = &self.0;
            #[cfg(target_os = "linux")]
            unsafe {
                libc::shm_unlink(std::ffi::CString::new(self.0.as_str()).unwrap().as_ptr());
            }
        }
    }

    #[test]
    fn second_daemon_cannot_replace_owner_or_reset_telemetry() {
        let name = unique_name("exclusive");
        let _cleanup = Cleanup(name.clone());
        let owner = open_named(true, &name).unwrap();
        owner
            .block()
            .peer_heartbeat_ns
            .store(123456, Ordering::Release);
        let observer = open_named(false, &name).unwrap();
        assert!(open_named(true, &name).is_err());
        assert_eq!(
            observer.block().daemon_pid.load(Ordering::Acquire),
            std::process::id()
        );
        assert_eq!(
            observer.block().peer_heartbeat_ns.load(Ordering::Acquire),
            123456
        );
        drop(owner);
        // Readers do not prevent replacing a daemon that has actually stopped.
        let replacement = open_named(true, &name).unwrap();
        assert_eq!(
            replacement.block().daemon_pid.load(Ordering::Acquire),
            std::process::id()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ownership_probe_waits_for_handle_release_without_resetting_state() {
        let name = unique_name("closing");
        let _cleanup = Cleanup(name.clone());
        let owner = open_named(true, &name).unwrap();
        let observer = open_named(false, &name).unwrap();
        // Model an exiting daemon whose PID is already gone but whose device
        // cleanup has not closed the ownership descriptor yet.
        owner.block().daemon_pid.store(0, Ordering::Release);
        owner.block().tx_hid_packets.store(72, Ordering::Release);
        assert!(!observer.daemon_ownership_available());
        assert!(!owner.daemon_ownership_available());
        assert!(open_named(true, &name).is_err());
        drop(owner);
        assert!(observer.daemon_ownership_available());
        assert_eq!(observer.block().tx_hid_packets.load(Ordering::Acquire), 72);
        let replacement = open_named(true, &name).unwrap();
        assert!(!observer.daemon_ownership_available());
        drop(replacement);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_live_owner_is_not_overwritten() {
        let name = unique_name("legacy");
        let _cleanup = Cleanup(name.clone());
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let owner = open_named(true, &name).unwrap();
        owner
            .block()
            .daemon_pid
            .store(child.id(), Ordering::Release);
        drop(owner); // Simulate a legacy daemon without the new kernel lock.
        let duplicate = open_named(true, &name);
        let result = duplicate.is_err();
        let reader = open_named(false, &name).unwrap();
        assert_eq!(
            reader.block().daemon_pid.load(Ordering::Acquire),
            child.id()
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            result,
            "must reject a live legacy daemon before resetting shared memory"
        );
        assert!(open_named(true, &name).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exited_unreaped_child_is_not_a_live_daemon() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if !is_process_alive(child.id()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let alive = is_process_alive(child.id());
        child.wait().unwrap();
        assert!(!alive, "zombie PID must not block daemon startup");
    }
}
