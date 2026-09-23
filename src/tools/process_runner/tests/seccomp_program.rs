// Split out of the old single-file test module; behavior is unchanged.

/// The seccomp filter's deny logic, simulated.
///
/// Jump arithmetic is the easiest thing in this file to get wrong by one
/// slot, and the platform it actually runs on is not the platform this
/// test runs on — so the program is interpreted here rather than trusted.
mod seccomp_program {
    use crate::tools::process_runner::seccomp::{
        network_filter_program, seccomp_constants, simulate, AF_INET6_NR, AF_INET_NR,
        SECCOMP_EPERM, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO,
    };

    const X86_64_ARCH: u32 = 0xC000_003E;
    const X86_64_SOCKET: u32 = 41;
    const NR_READ: u32 = 0;

    #[test]
    fn internet_sockets_are_denied_and_unix_sockets_are_not() {
        let prog = network_filter_program(X86_64_ARCH, X86_64_SOCKET);
        let run = |arch: u32, nr: u32, arg0: u32| simulate(&prog, arch, nr, arg0);
        let denied = SECCOMP_RET_ERRNO | SECCOMP_EPERM;

        assert_eq!(run(X86_64_ARCH, X86_64_SOCKET, AF_INET_NR), denied);
        assert_eq!(run(X86_64_ARCH, X86_64_SOCKET, AF_INET6_NR), denied);

        // A network posture is not a reason to break local IPC.
        assert_eq!(run(X86_64_ARCH, X86_64_SOCKET, 1), SECCOMP_RET_ALLOW);
        // Other syscalls are untouched — this is a network clause, not a jail.
        assert_eq!(run(X86_64_ARCH, NR_READ, AF_INET_NR), SECCOMP_RET_ALLOW);
        // A filter built for another architecture must not match ours.
        assert_eq!(run(0xDEAD_BEEF, X86_64_SOCKET, AF_INET_NR), SECCOMP_RET_ALLOW);
    }

    /// The per-architecture constants are the whole reason the filter takes
    /// them as parameters; a wrong `__NR_socket` would silently deny (or
    /// not deny) the wrong syscall. `libc` only carries the AUDIT_ARCH
    /// constants on Linux and this test only runs on macOS, so the table
    /// is asserted against its documented derivation instead.
    #[test]
    fn architecture_constants_are_the_documented_ones() {
        const EM_X86_64: u32 = 62;
        const EM_AARCH64: u32 = 183;
        const AUDIT_FLAGS: u32 = 0x8000_0000 | 0x4000_0000; // 64BIT | LE

        assert_eq!(seccomp_constants("x86_64"), Some((EM_X86_64 | AUDIT_FLAGS, 41)));
        assert_eq!(seccomp_constants("aarch64"), Some((EM_AARCH64 | AUDIT_FLAGS, 198)));
        // An architecture the filter has no numbers for must refuse to
        // install rather than guess.
        assert_eq!(seccomp_constants("riscv64"), None);
        assert_eq!(seccomp_constants(""), None);

        let (aarch64_arch, aarch64_socket) = seccomp_constants("aarch64").expect("aarch64");
        assert_eq!(X86_64_ARCH, EM_X86_64 | AUDIT_FLAGS);
        assert_eq!(X86_64_SOCKET, 41);

        let aarch64 = network_filter_program(aarch64_arch, aarch64_socket);
        assert_eq!(
            simulate(&aarch64, aarch64_arch, aarch64_socket, AF_INET_NR),
            SECCOMP_RET_ERRNO | SECCOMP_EPERM
        );
    }
}
