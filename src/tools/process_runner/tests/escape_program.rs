// Split out of the old single-file test module; behavior is unchanged.

/// [`seccomp::simulate`] for why). Ungated on purpose: unlike the network
/// posture — whose tests live behind the macOS-authored `seccomp_program`
/// module — this filter rides on every fenced Linux run, so CI's ubuntu
/// runner must exercise it too, not just the dev mac.
mod escape_program {
    use crate::tools::process_runner::seccomp::{
        escape_constants, escape_filter_program, simulate, EscapeTable, CLONE_NEW_MASK,
        SECCOMP_EACCES, SECCOMP_ENOSYS, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO,
    };

    const NR_EXECVE_X86: u32 = 59;
    const NR_EXECVE_AARCH64: u32 = 221;
    const NR_OPENAT_X86: u32 = 257;
    const SIGCHLD: u32 = 17;
    const CLONE_FS: u32 = 0x0000_0200;

    fn table(arch: &str) -> EscapeTable {
        escape_constants(arch).unwrap_or_else(|| panic!("{arch} must have an escape table"))
    }

    #[test]
    fn every_deny_list_entry_is_denied_with_eacces() {
        for arch in ["x86_64", "aarch64"] {
            let table = table(arch);
            let prog = escape_filter_program(&table);
            let denied = SECCOMP_RET_ERRNO | SECCOMP_EACCES;
            for (name, nr) in table.denied {
                assert_eq!(
                    simulate(&prog, table.audit_arch, *nr, 0),
                    denied,
                    "{arch}: {name} ({nr}) must be denied"
                );
            }
        }
    }

    #[test]
    fn ordinary_work_passes_untouched() {
        let t = table("x86_64");
        let prog = escape_filter_program(&t);
        let run = |nr: u32, arg0: u32| simulate(&prog, t.audit_arch, nr, arg0);

        assert_eq!(run(NR_EXECVE_X86, 0), SECCOMP_RET_ALLOW);
        assert_eq!(run(NR_OPENAT_X86, 0), SECCOMP_RET_ALLOW);
        assert_eq!(run(0, 0), SECCOMP_RET_ALLOW, "read");
        // clone without any CLONE_NEW* flag is just a fork.
        assert_eq!(run(t.nr_clone, SIGCHLD), SECCOMP_RET_ALLOW);
        assert_eq!(run(t.nr_clone, SIGCHLD | CLONE_FS), SECCOMP_RET_ALLOW);

        let a = table("aarch64");
        let prog = escape_filter_program(&a);
        assert_eq!(simulate(&prog, a.audit_arch, NR_EXECVE_AARCH64, 0), SECCOMP_RET_ALLOW);
    }

    #[test]
    fn clone3_is_refused_so_libc_falls_back_to_filterable_clone() {
        for arch in ["x86_64", "aarch64"] {
            let t = table(arch);
            let prog = escape_filter_program(&t);
            assert_eq!(
                simulate(&prog, t.audit_arch, t.nr_clone3, 0),
                SECCOMP_RET_ERRNO | SECCOMP_ENOSYS,
                "{arch}: clone3"
            );
        }
    }

    #[test]
    fn namespaced_clone_is_denied_by_flags_not_number() {
        for arch in ["x86_64", "aarch64"] {
            let t = table(arch);
            let prog = escape_filter_program(&t);
            let run = |flags: u32| simulate(&prog, t.audit_arch, t.nr_clone, flags);
            let denied = SECCOMP_RET_ERRNO | SECCOMP_EACCES;

            assert_eq!(run(CLONE_NEW_MASK), denied, "{arch}: full mask");
            // Each namespace bit on its own — a single wrong constant in
            // the mask must not hide behind the rest of the mask.
            for bit in [
                0x0002_0000u32,
                0x0200_0000,
                0x0400_0000,
                0x0800_0000,
                0x1000_0000,
                0x2000_0000,
                0x4000_0000,
            ] {
                assert_eq!(run(bit), denied, "{arch}: CLONE_NEW bit {bit:#x}");
            }
        }
    }

    #[test]
    fn wrong_architecture_lane_allows() {
        let t = table("x86_64");
        let prog = escape_filter_program(&t);
        let (_, nr) = t.denied[0];
        assert_eq!(
            simulate(&prog, 0xDEAD_BEEF, nr, 0),
            SECCOMP_RET_ALLOW,
            "a program built for one arch must not deny another's syscalls"
        );
    }

    #[test]
    fn tables_are_well_formed() {
        for arch in ["x86_64", "aarch64"] {
            let t = table(arch);
            assert!(t.denied.len() >= 35, "{arch}: deny list suspiciously short");
            let mut seen = Vec::new();
            for (name, nr) in t.denied {
                assert!(!seen.contains(nr), "{arch}: duplicate number for {name}");
                seen.push(*nr);
            }
            assert_ne!(t.nr_clone, t.nr_clone3, "{arch}");
        }
        // An architecture the table has no numbers for must come back
        // None so the installer fails closed.
        assert!(escape_constants("riscv64").is_none());
        assert!(escape_constants("").is_none());
    }
}
