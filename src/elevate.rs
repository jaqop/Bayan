//! One check for "is this process token elevated?".
//!
//! `--elevated` (PR #56) and Explorer's Run as administrator both land here:
//! `TokenElevation` on the process token, not a config key. PR #56 should
//! call [`is_elevated`] and drop its own copy of this query.
//!
//! A failed query is treated as elevated. Scripts and plugins stay off when
//! we cannot prove the token is limited.

/// `opened` is `OpenProcessToken` succeeding. `query_ok` is
/// `GetTokenInformation` succeeding. `bit` is `TokenIsElevated`.
pub fn elevated_from_query(opened: bool, query_ok: bool, bit: bool) -> bool {
    if !opened || !query_ok {
        return true;
    }
    bit
}

/// Whether this process token is elevated.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        token_is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn token_is_elevated() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: GetCurrentProcess returns a pseudo-handle that is not closed.
    // OpenProcessToken writes one token handle, and CloseHandle releases it.
    // GetTokenInformation writes into `elevation`; the size is the struct's.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return elevated_from_query(false, false, false);
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut written = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        );
        let _ = CloseHandle(token);
        elevated_from_query(true, ok != 0, elevation.TokenIsElevated != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_elevation_query_is_treated_as_elevated() {
        assert!(elevated_from_query(false, false, false));
        assert!(elevated_from_query(false, true, false));
        assert!(elevated_from_query(true, false, false));
        assert!(!elevated_from_query(true, true, false));
        assert!(elevated_from_query(true, true, true));
    }
}
