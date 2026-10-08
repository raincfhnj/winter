use std::iter;

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, SetLastError, WIN32_ERROR,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{Owned, PCWSTR};

use super::error::{PlatformError, PlatformResult};

pub const DEFAULT_INSTANCE_MUTEX_NAME: &str =
    r"Local\Winter.Controller.7eb6e94d-5a26-42d7-856a-97e878aa344f";

/// Owned named mutex proving this is the controller instance for the session.
pub struct SingleInstanceGuard {
    _handle: Owned<HANDLE>,
}

impl SingleInstanceGuard {
    pub fn acquire() -> PlatformResult<Self> {
        Self::acquire_named(DEFAULT_INSTANCE_MUTEX_NAME)
    }

    pub fn acquire_named(name: &str) -> PlatformResult<Self> {
        let wide_name =
            nul_terminated(name).ok_or_else(|| PlatformError::InvalidMutexName(name.to_owned()))?;

        // SAFETY: this only resets the calling thread's last-error value before
        // CreateMutexW so ERROR_ALREADY_EXISTS can be read unambiguously.
        unsafe { SetLastError(WIN32_ERROR(0)) };
        // Handle existence, rather than mutex ownership, is the lifetime lock.
        // This keeps the guard movable across controller threads and avoids
        // requiring `ReleaseMutex` on the thread that created it.
        // SAFETY: `wide_name` is a live, immutable, NUL-terminated UTF-16
        // buffer for the duration of CreateMutexW.
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide_name.as_ptr())) }
            .map_err(|source| PlatformError::win32("CreateMutexW", source))?;
        // SAFETY: this reads the calling thread's last-error value immediately
        // after CreateMutexW.
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: CreateMutexW returned a valid, newly acquired handle and this
        // guard becomes its sole Rust owner.
        let handle = unsafe { Owned::new(handle) };
        if already_exists {
            return Err(PlatformError::AlreadyRunning);
        }

        Ok(Self { _handle: handle })
    }
}

fn nul_terminated(value: &str) -> Option<Vec<u16>> {
    if value.is_empty() || value.encode_utf16().any(|unit| unit == 0) {
        return None;
    }

    Some(value.encode_utf16().chain(iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_MUTEX: AtomicU64 = AtomicU64::new(1);

    fn unique_mutex_name() -> String {
        format!(
            r"Local\Winter.Tests.{}.{}",
            std::process::id(),
            NEXT_MUTEX.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn rejects_empty_or_embedded_nul_mutex_names() {
        assert!(matches!(
            SingleInstanceGuard::acquire_named(""),
            Err(PlatformError::InvalidMutexName(_))
        ));
        assert!(matches!(
            SingleInstanceGuard::acquire_named("Local\\bad\0name"),
            Err(PlatformError::InvalidMutexName(_))
        ));
    }

    #[test]
    fn second_guard_is_rejected_until_first_is_dropped() {
        let name = unique_mutex_name();
        let first = SingleInstanceGuard::acquire_named(&name).expect("first guard");
        assert!(matches!(
            SingleInstanceGuard::acquire_named(&name),
            Err(PlatformError::AlreadyRunning)
        ));

        drop(first);
        SingleInstanceGuard::acquire_named(&name).expect("mutex released after drop");
    }
}
