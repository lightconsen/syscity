//! The escape table's hardcoded numbers against the libc constants, and
//! the kernel-verifier acceptance check for both seccomp programs.

use crate::tools::process_runner::seccomp::{
    escape_constants, install_escape_seccomp, install_network_seccomp,
};

fn libc_nr(name: &str) -> libc::c_long {
    match name {
        "ptrace" => libc::SYS_ptrace,
        "process_vm_readv" => libc::SYS_process_vm_readv,
        "process_vm_writev" => libc::SYS_process_vm_writev,
        "kcmp" => libc::SYS_kcmp,
        "process_madvise" => libc::SYS_process_madvise,
        "bpf" => libc::SYS_bpf,
        "perf_event_open" => libc::SYS_perf_event_open,
        "userfaultfd" => libc::SYS_userfaultfd,
        "kexec_load" => libc::SYS_kexec_load,
        "kexec_file_load" => libc::SYS_kexec_file_load,
        "open_by_handle_at" => libc::SYS_open_by_handle_at,
        "name_to_handle_at" => libc::SYS_name_to_handle_at,
        "lookup_dcookie" => libc::SYS_lookup_dcookie,
        // x86-only port I/O: the syscalls (and libc's constants for
        // them) do not exist on ARM64, where the escape table does
        // not list them either.
        #[cfg(target_arch = "x86_64")]
        "ioperm" => libc::SYS_ioperm,
        #[cfg(target_arch = "x86_64")]
        "iopl" => libc::SYS_iopl,
        "swapon" => libc::SYS_swapon,
        "swapoff" => libc::SYS_swapoff,
        "quotactl" => libc::SYS_quotactl,
        "acct" => libc::SYS_acct,
        "reboot" => libc::SYS_reboot,
        "keyctl" => libc::SYS_keyctl,
        "add_key" => libc::SYS_add_key,
        "request_key" => libc::SYS_request_key,
        "init_module" => libc::SYS_init_module,
        "finit_module" => libc::SYS_finit_module,
        "delete_module" => libc::SYS_delete_module,
        "mount" => libc::SYS_mount,
        "umount2" => libc::SYS_umount2,
        "pivot_root" => libc::SYS_pivot_root,
        "unshare" => libc::SYS_unshare,
        "setns" => libc::SYS_setns,
        "fsopen" => libc::SYS_fsopen,
        "fsconfig" => libc::SYS_fsconfig,
        "fsmount" => libc::SYS_fsmount,
        "fspick" => libc::SYS_fspick,
        "move_mount" => libc::SYS_move_mount,
        "open_tree" => libc::SYS_open_tree,
        "mount_setattr" => libc::SYS_mount_setattr,
        "remap_file_pages" => libc::SYS_remap_file_pages,
        other => unreachable!("escape-table entry without a libc mapping: {other}"),
    }
}

#[test]
fn escape_syscall_tables_match_libc() {
    // libc's SYS_* constants describe the architecture this test
    // compiles for, so only the running arch's table can be checked
    // against them; the other tables are covered by the simulator's
    // documented-derivation tests.
    let arch = std::env::consts::ARCH;
    let t = escape_constants(arch).expect("the compiling arch must have a table");
    for (name, nr) in t.denied {
        assert_eq!(*nr as libc::c_long, libc_nr(name), "{arch}: {name}");
    }
    assert_eq!(t.nr_clone as libc::c_long, libc::SYS_clone, "{arch}");
    assert_eq!(t.nr_clone3 as libc::c_long, libc::SYS_clone3, "{arch}");
}

/// The kernel's seccomp verifier enforces structural rules the simulator
/// does not model (instruction whitelist, jump-shape checks), so the real
/// programs must be installed on the real kernel once. seccomp filters
/// apply to the calling thread only — sibling tests are untouched.
#[cfg(target_os = "linux")]
#[test]
fn seccomp_programs_pass_the_kernel_verifier() {
    install_escape_seccomp().expect("kernel must accept the escape-vector filter");
    install_network_seccomp().expect("kernel must accept the network filter");
}
