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
        use windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let ok = TerminateProcess(handle, 1).is_ok();
            let _ = CloseHandle(handle);
            ok
        } else {
            false
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

    #[cfg(target_os = "linux")]
    fn open_linux(create_and_init: bool) -> Result<Self, PalError> {
        use duplex_proto::LINUX_SHM_NAME;
        use std::ffi::CString;

        let c_name = CString::new(LINUX_SHM_NAME).unwrap();
        let flags = if create_and_init {
            libc::O_CREAT | libc::O_RDWR
        } else {
            libc::O_RDWR
        };

        let fd = unsafe { libc::shm_open(c_name.as_ptr(), flags, 0o660) };
        if fd < 0 {
            return Err(PalError::SharedMemory(format!(
                "shm_open({}) failed: {}",
                LINUX_SHM_NAME,
                std::io::Error::last_os_error()
            )));
        }

        let size = core::mem::size_of::<ShmWatchdogBlock>();
        if create_and_init {
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
            let pid = std::process::id();
            shm.block().init_daemon(pid, monotonic_now_ns());
        }

        Ok(shm)
    }

    #[cfg(target_os = "windows")]
    fn open_windows(create_and_init: bool) -> Result<Self, PalError> {
        use duplex_proto::WINDOWS_SHM_NAME;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
        };

        let wide_name: Vec<u16> = WINDOWS_SHM_NAME
            .encode_utf16()
            .chain(core::iter::once(0))
            .collect();
        let size = core::mem::size_of::<ShmWatchdogBlock>() as u32;

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
        };

        if create_and_init {
            let pid = std::process::id();
            shm.block().init_daemon(pid, monotonic_now_ns());
        }

        Ok(shm)
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
            use windows::Win32::System::Memory::{
                UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
            };
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr.as_ptr() as *mut core::ffi::c_void,
            });
            let _ = CloseHandle(self.handle);
            let _ = self.is_owner;
        }
    }
}
