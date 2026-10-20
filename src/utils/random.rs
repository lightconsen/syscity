//! OS-entropy helpers.
//!
//! rand 0.10 split the operating system's random source away from the
//! infallible `Rng` trait: `rand::rngs::SysRng` implements only the fallible
//! `TryRng`, and rand 0.8's infallible `OsRng` no longer exists. These two
//! functions are the replacement for the `OsRng::fill_bytes` call sites.
//!
//! They deliberately use `SysRng` rather than `rand::rng()`: the latter is a
//! thread-local ChaCha generator seeded from the OS, which is secure but is
//! not the OS source itself. The callers here mint session tokens, OAuth PKCE
//! verifiers and gateway tokens, and asked for OS entropy explicitly.

use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
use rand::Rng as _;

/// Fill `dst` with bytes from the operating system's random source.
///
/// Panics if the OS source is unavailable — the same behaviour as rand 0.8's
/// `OsRng::fill_bytes`, which this replaces. Prefer [`try_fill_os_entropy`]
/// where the caller can report the failure instead.
pub fn fill_os_entropy(dst: &mut [u8]) {
    UnwrapErr(SysRng).fill_bytes(dst);
}

/// Fill `dst` with bytes from the operating system's random source, reporting
/// failure rather than panicking.
pub fn try_fill_os_entropy(dst: &mut [u8]) -> crate::Result<()> {
    use rand::TryRng as _;

    SysRng.try_fill_bytes(dst).map_err(|e| {
        crate::error::SyscityError::Internal(format!("OS entropy source unavailable: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_every_byte() {
        // A zeroed buffer that comes back zeroed would mean the call silently
        // did nothing; two draws agreeing on all 32 bytes would mean it is not
        // drawing at all. (2^-256, so this cannot flake.)
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill_os_entropy(&mut a);
        fill_os_entropy(&mut b);
        assert_ne!(a, [0u8; 32]);
        assert_ne!(a, b);
    }

    #[test]
    fn try_variant_fills_and_reports_success() {
        let mut buf = [0u8; 32];
        try_fill_os_entropy(&mut buf).expect("the OS source is available in tests");
        assert_ne!(buf, [0u8; 32]);
    }
}
