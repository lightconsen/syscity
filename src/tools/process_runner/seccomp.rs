//! The seccomp filters for the Linux fence: a libc-free classic-BPF
//! assembler, the network-posture and escape-vector programs, the
//! per-architecture syscall tables, and the in-child installers.
//!
//! Split out of the old single-file `process_runner`; behavior is unchanged.

/// One classic-BPF instruction, in the shape the kernel takes but free of
/// libc types so the program can be built and simulated on any platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BpfInsn {
    pub(super) code: u16,
    pub(super) jt: u8,
    pub(super) jf: u8,
    pub(super) k: u32,
}

// Classic-BPF encoding (linux/bpf_common.h, linux/filter.h).
pub(super) const BPF_LD: u16 = 0x00;
pub(super) const BPF_W: u16 = 0x00;
pub(super) const BPF_ABS: u16 = 0x20;
pub(super) const BPF_JMP: u16 = 0x05;
pub(super) const BPF_JEQ: u16 = 0x10;
pub(super) const BPF_K: u16 = 0x00;
pub(super) const BPF_RET: u16 = 0x06;
// seccomp return values (linux/seccomp.h).
pub(super) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub(super) const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
pub(super) const SECCOMP_EPERM: u32 = 1;
// struct seccomp_data offsets: nr at 0, arch at 4, args[0] at 16.
pub(super) const SECCOMP_OFF_NR: u32 = 0;
pub(super) const SECCOMP_OFF_ARCH: u32 = 4;
pub(super) const SECCOMP_OFF_ARG0: u32 = 16;
pub(super) const AF_INET_NR: u32 = 2;
pub(super) const AF_INET6_NR: u32 = 10;

/// The `[security] fence_network` filter, as an instruction list.
///
/// Denies `socket(2)` for `AF_INET`/`AF_INET6` with `EPERM` and allows
/// everything else — in particular `AF_UNIX`, because unix sockets are how a
/// great deal of ordinary local tooling talks (dbus, systemd, docker) and a
/// network posture is not a reason to break them. Denying the socket's
/// creation is enough on its own: the child inherits only its stdio
/// descriptors, so there is no pre-existing internet socket to fall back on.
///
/// `audit_arch` and `nr_socket` are parameters because both are
/// architecture-specific and the filter validates them: a program built for
/// one architecture must not be interpreted as another.
pub(super) fn network_filter_program(audit_arch: u32, nr_socket: u32) -> Vec<BpfInsn> {
    let stmt = |code: u16, jt: u8, jf: u8, k: u32| BpfInsn { code, jt, jf, k };
    // Jump offsets resolved by hand against this exact list:
    //   arch != ours                   -> allow
    //   nr != socket                   -> allow
    //   domain == AF_INET || AF_INET6  -> EPERM
    //   otherwise                      -> allow
    vec![
        stmt(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARCH),
        // wrong architecture: not our syscall numbers, so allow — six slots
        // down, landing on the final allow (not the EPERM before it)
        stmt(BPF_JMP | BPF_JEQ | BPF_K, 0, 6, audit_arch),
        stmt(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_NR),
        // not socket(2): allow, four slots down
        stmt(BPF_JMP | BPF_JEQ | BPF_K, 0, 4, nr_socket),
        stmt(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARG0),
        // AF_INET: skip the AF_INET6 test, landing on the EPERM
        stmt(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, AF_INET_NR),
        // AF_INET6: EPERM; anything else: the allow below
        stmt(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, AF_INET6_NR),
        stmt(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | SECCOMP_EPERM),
        stmt(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW),
    ]
}

/// `(AUDIT_ARCH, __NR_socket)` for a target architecture, or `None` when the
/// filter has no numbers for it.
///
/// A function rather than a `cfg`-gated constant so every entry can be
/// asserted from any host: the architecture this runs on and the architecture
/// the test runs on are rarely the same machine.
pub(super) fn seccomp_constants(arch: &str) -> Option<(u32, u32)> {
    match arch {
        // EM_X86_64 (62) | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE, and
        // `__NR_socket` from asm/unistd_64.h.
        "x86_64" => Some((0xC000_003E, 41)),
        // EM_AARCH64 (183) | the same two flags, and `__NR_socket` from
        // asm-generic/unistd.h.
        "aarch64" => Some((0xC000_00B7, 198)),
        _ => None,
    }
}

// The escape-vector filter's return errnos. EACCES, not EPERM: the
// escalation classifier (`escalation::fence_denial_reason`) deliberately
// treats "Operation not permitted" as *the fence refused, offer to re-run
// outside it* — the right recovery for a blocked network connect, but the
// wrong recovery for `ptrace`/`mount`, which are never legitimate work for
// a fenced command and must stay silently refused. EACCES ("Permission
// denied") is unclassified for exactly that reason (see the Landlock note
// there). ENOSYS for `clone3` makes libc fall back to filterable `clone`.
pub(super) const SECCOMP_EACCES: u32 = 13;
pub(super) const SECCOMP_ENOSYS: u32 = 38;
// Classic-BPF jump opcodes (linux/bpf_common.h): the op field is bits
// 4-6 (0x70) — JEQ 0x10, JSET 0x40 — and bit 3 (0x08) is the K/X source
// selector, not an opcode.
pub(super) const BPF_JSET: u16 = 0x40;

/// Every syscall number the escape filter needs, per architecture.
///
/// Hardcoded for the same reason [`seccomp_constants`] is: the numbers must
/// be visible to the simulator on the host the tests run on. On Linux a
/// contract test (`escape_syscall_table_matches_libc`) asserts every entry
/// against the `libc::SYS_*` constant of the same name, so a typo here
/// fails CI rather than silently denying the wrong syscall.
#[derive(Debug, Clone, Copy)]
pub(super) struct EscapeTable {
    pub(super) audit_arch: u32,
    /// `(name, __NR_*)` pairs denied with EACCES.
    pub(super) denied: &'static [(&'static str, u32)],
    pub(super) nr_clone: u32,
    pub(super) nr_clone3: u32,
}

/// The one mask that means "this clone is a namespace escape attempt".
///
/// `NEWNS | NEWCGROUP | NEWUTS | NEWIPC | NEWUSER | NEWPID | NEWNET`
/// (linux/sched.h) — values are architecture-independent.
pub(super) const CLONE_NEW_MASK: u32 = 0x7E02_0000;

/// `None` on architectures the filter has no numbers for — the installer
/// fails closed rather than guess.
pub(super) fn escape_constants(arch: &str) -> Option<EscapeTable> {
    match arch {
        // asm/unistd_64.h. `ioperm`/`iopl` are x86-only — no I/O ports on
        // ARM64, so they are simply absent from this table.
        "x86_64" => Some(EscapeTable {
            audit_arch: 0xC000_003E,
            denied: &[
                ("ptrace", 101),
                ("process_vm_readv", 310),
                ("process_vm_writev", 311),
                ("kcmp", 312),
                ("process_madvise", 440),
                ("bpf", 321),
                ("perf_event_open", 298),
                ("userfaultfd", 323),
                ("kexec_load", 246),
                ("kexec_file_load", 320),
                ("open_by_handle_at", 304),
                ("name_to_handle_at", 303),
                ("lookup_dcookie", 212),
                ("ioperm", 173),
                ("iopl", 172),
                ("swapon", 167),
                ("swapoff", 168),
                ("quotactl", 179),
                ("acct", 163),
                ("reboot", 169),
                ("keyctl", 250),
                ("add_key", 248),
                ("request_key", 249),
                ("init_module", 175),
                ("finit_module", 313),
                ("delete_module", 176),
                ("mount", 165),
                ("umount2", 166),
                ("pivot_root", 155),
                ("unshare", 272),
                ("setns", 308),
                ("fsopen", 430),
                ("fsconfig", 431),
                ("fsmount", 432),
                ("fspick", 433),
                ("move_mount", 429),
                ("open_tree", 428),
                ("mount_setattr", 442),
                ("remap_file_pages", 216),
            ],
            nr_clone: 56,
            nr_clone3: 435,
        }),
        // asm-generic/unistd.h.
        "aarch64" => Some(EscapeTable {
            audit_arch: 0xC000_00B7,
            denied: &[
                ("ptrace", 117),
                ("process_vm_readv", 270),
                ("process_vm_writev", 271),
                ("kcmp", 272),
                ("process_madvise", 440),
                ("bpf", 280),
                ("perf_event_open", 241),
                ("userfaultfd", 282),
                ("kexec_load", 104),
                ("kexec_file_load", 294),
                ("open_by_handle_at", 265),
                ("name_to_handle_at", 264),
                ("lookup_dcookie", 18),
                ("swapon", 224),
                ("swapoff", 225),
                ("quotactl", 60),
                ("acct", 89),
                ("reboot", 142),
                ("keyctl", 219),
                ("add_key", 217),
                ("request_key", 218),
                ("init_module", 105),
                ("finit_module", 273),
                ("delete_module", 106),
                ("mount", 40),
                ("umount2", 39),
                ("pivot_root", 41),
                ("unshare", 97),
                ("setns", 268),
                ("fsopen", 430),
                ("fsconfig", 431),
                ("fsmount", 432),
                ("fspick", 433),
                ("move_mount", 429),
                ("open_tree", 428),
                ("mount_setattr", 442),
                ("remap_file_pages", 234),
            ],
            nr_clone: 220,
            nr_clone3: 435,
        }),
        _ => None,
    }
}

/// Named fixup targets for the two-pass assembler below. Jump offsets in a
/// ~50-instruction deny chain are unmanageable by hand — each conditional
/// jump names the label it jumps to and the builder patches real offsets at
/// `resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    DenyEacces,
    DenyEnosys,
    /// Wrong-architecture lane.
    Allow,
    /// A syscall that matched no chain entry.
    ChainAllow,
    /// A `clone` whose flags carry no `CLONE_NEW*` bit.
    FlagAllow,
    CloneFlags,
}

/// Tiny classic-BPF assembler: emit instructions, name forward jumps,
/// resolve on build. Classic-BPF jump offsets are unsigned — every jump
/// must land *after* its source — so the program below is laid out with
/// its decision points behind the chain, and `resolve` fails loudly on a
/// distance that does not fit.
struct ProgramBuilder {
    program: Vec<BpfInsn>,
    jt_fixups: Vec<(usize, Label)>,
    jf_fixups: Vec<(usize, Label)>,
    labels: Vec<(Label, usize)>,
}

impl ProgramBuilder {
    fn new() -> Self {
        Self {
            program: Vec::new(),
            jt_fixups: Vec::new(),
            jf_fixups: Vec::new(),
            labels: Vec::new(),
        }
    }

    fn emit(&mut self, code: u16, jt: u8, jf: u8, k: u32) -> usize {
        self.program.push(BpfInsn { code, jt, jf, k });
        self.program.len() - 1
    }

    fn mark(&mut self, label: Label) {
        self.labels.push((label, self.program.len()));
    }

    /// `JEQ K`: jump to `target` when the accumulator equals `k`, fall
    /// through to the next instruction otherwise.
    fn jeq(&mut self, k: u32, target: Label) {
        let at = self.emit(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, k);
        self.jt_fixups.push((at, target));
    }

    /// `JEQ K` that falls through when equal and jumps when not — the
    /// architecture gate, whose mismatch lane is the exception.
    fn jeq_else(&mut self, k: u32, not_equal: Label) {
        let at = self.emit(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, k);
        self.jf_fixups.push((at, not_equal));
    }

    /// `JSET K`: jump to `hit` when `accumulator & k` is non-zero, jump to
    /// `miss` otherwise.
    fn jset(&mut self, k: u32, hit: Label, miss: Label) {
        let at = self.emit(BPF_JMP | BPF_JSET | BPF_K, 0, 0, k);
        self.jt_fixups.push((at, hit));
        self.jf_fixups.push((at, miss));
    }

    /// Position of a marked label. Every label used by a jump is marked
    /// before `resolve`; an unmarked one is an assembler bug the
    /// simulator tests catch, so panicking is the correct behavior.
    #[allow(clippy::expect_used)] // assembler invariant, not input validation
    fn position(&self, label: Label) -> usize {
        self.labels
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, at)| *at)
            .expect("label marked")
    }

    /// Classic-BPF jump offsets are single bytes; a distance that does
    /// not fit means the program layout grew past what one filter can
    /// express, and panicking here is the point (the tests would catch
    /// it long before a kernel sees the program).
    #[allow(clippy::expect_used)] // layout bug, not input validation
    fn resolve(mut self) -> Vec<BpfInsn> {
        let jt_fixups = std::mem::take(&mut self.jt_fixups);
        let jf_fixups = std::mem::take(&mut self.jf_fixups);
        for (at, label) in jt_fixups {
            let off = u8::try_from(self.position(label) - at - 1)
                .expect("jump distance exceeds one byte");
            self.program[at].jt = off;
        }
        for (at, label) in jf_fixups {
            let off = u8::try_from(self.position(label) - at - 1)
                .expect("jump distance exceeds one byte");
            self.program[at].jf = off;
        }
        self.program
    }
}

/// The escape-vector filter, as an instruction list.
///
/// Architecture gate → linear syscall chain (one `JEQ` per denied call,
/// hit = EACCES) → allow. `clone` is pulled out of the chain: its denial
/// is conditional on carrying a `CLONE_NEW*` flag, checked with `JSET` on
/// `args[0]`; `clone3` is denied with `ENOSYS` outright so libc falls back
/// to a `clone` whose flags this filter can actually see. Ordinary work —
/// `execve`, `openat`, an un-namespaced `clone` — passes untouched: this
/// is an escape-vector deny list, not a jail. (There are three allow
/// returns rather than one because classic-BPF jumps only go forward.)
pub(super) fn escape_filter_program(table: &EscapeTable) -> Vec<BpfInsn> {
    let mut b = ProgramBuilder::new();
    // Load and check the audit architecture first: a program built for one
    // architecture must not deny another's syscalls (same argument as
    // [`network_filter_program`]; at runtime the numbers come from
    // `std::env::consts::ARCH`, so the mismatch lane is unreachable and
    // errs toward allowing).
    b.emit(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARCH);
    b.jeq_else(table.audit_arch, Label::Allow);
    b.emit(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_NR);
    for (_, nr) in table.denied {
        b.jeq(*nr, Label::DenyEacces);
    }
    b.jeq(table.nr_clone3, Label::DenyEnosys);
    b.jeq(table.nr_clone, Label::CloneFlags);
    // No chain entry matched: an ordinary syscall.
    b.mark(Label::ChainAllow);
    b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW);
    b.mark(Label::CloneFlags);
    // clone: the decision rides on the flags, args[0] in seccomp_data.
    b.emit(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARG0);
    b.jset(CLONE_NEW_MASK, Label::DenyEacces, Label::FlagAllow);
    b.mark(Label::DenyEacces);
    b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | SECCOMP_EACCES);
    b.mark(Label::DenyEnosys);
    b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | SECCOMP_ENOSYS);
    b.mark(Label::Allow);
    b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW);
    b.mark(Label::FlagAllow);
    b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW);
    b.resolve()
}

/// Walk the program the way the kernel would, for one syscall.
///
/// Test-only: jump arithmetic is the easiest thing in this file to get
/// wrong by one slot, so the programs are interpreted here rather than
/// trusted. Lives in this module (not in a per-platform test module) so
/// both the network and the escape program can be simulated on any host.
#[cfg(test)]
pub(super) fn simulate(prog: &[BpfInsn], arch: u32, nr: u32, arg0: u32) -> u32 {
    let mut a = 0u32;
    let mut pc = 0usize;
    loop {
        let insn = prog[pc];
        match insn.code & 0x07 {
            0x00 => {
                a = match insn.k {
                    SECCOMP_OFF_ARCH => arch,
                    SECCOMP_OFF_NR => nr,
                    SECCOMP_OFF_ARG0 => arg0,
                    other => panic!("unexpected load offset {other}"),
                };
                pc += 1;
            }
            0x05 => {
                // The op field is bits 4-6: JEQ (0x10) and JSET (0x40)
                // both live in the JMP class and differ by op bits.
                let taken = match insn.code & 0x70 {
                    BPF_JSET => a & insn.k != 0,
                    // BPF_JEQ (and every other jump, which the programs
                    // here do not emit).
                    _ => a == insn.k,
                };
                pc += 1 + if taken {
                    insn.jt as usize
                } else {
                    insn.jf as usize
                };
            }
            0x06 => return insn.k,
            other => panic!("unexpected instruction class {other}"),
        }
    }
}

/// Linux: install [`network_filter_program`] on the calling thread.
///
/// Runs in the child after the Landlock restrict, which has already set
/// `no_new_privs` — the kernel refuses a filter without it.
#[cfg(target_os = "linux")]
pub(super) fn install_network_seccomp() -> std::io::Result<()> {
    // Fail closed rather than install a filter whose syscall numbers are
    // guesses: a network posture that silently does nothing is worse than one
    // that refuses to start. `consts::ARCH` is the compile-time target, so
    // this cannot mismatch the binary.
    let Some((audit_arch, nr_socket)) = seccomp_constants(std::env::consts::ARCH) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no seccomp network filter for this architecture",
        ));
    };
    install_seccomp_program(&network_filter_program(audit_arch, nr_socket))
}

/// Linux: install [`escape_filter_program`] on the calling thread.
///
/// Unlike the network posture this is not optional: every fenced run gets the
/// escape-vector deny list, because `ptrace` and friends are not work a fenced
/// command has any legitimate use for. Unknown architectures fail closed,
/// matching the network filter.
#[cfg(target_os = "linux")]
pub(super) fn install_escape_seccomp() -> std::io::Result<()> {
    let Some(table) = escape_constants(std::env::consts::ARCH) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no seccomp escape filter for this architecture",
        ));
    };
    install_seccomp_program(&escape_filter_program(&table))
}

/// Convert a `BpfInsn` program to the kernel's `sock_fprog` shape and load it
/// with `seccomp(SECCOMP_SET_MODE_FILTER)`.
#[cfg(target_os = "linux")]
fn install_seccomp_program(program: &[BpfInsn]) -> std::io::Result<()> {
    use libc::{sock_filter, sock_fprog};

    const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;

    {
        let filter: Vec<sock_filter> = program
            .iter()
            .map(|i| sock_filter {
                code: i.code,
                jt: i.jt,
                jf: i.jf,
                k: i.k,
            })
            .collect();
        let mut prog = sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr() as *mut sock_filter,
        };

        // `no_new_privs` is set by the Landlock restrict before this runs;
        // repeating it is idempotent and keeps the filter installable if the
        // order ever changes.
        #[allow(unsafe_code)] // prctl has no safe wrapper in libc; no memory is touched
        let prctl_rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if prctl_rc != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: `prog` points at `filter`, which outlives this call, and the
        // kernel copies the program during the syscall — nothing is retained.
        // Variadic args are widened explicitly (see the mount_setattr note).
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                SECCOMP_SET_MODE_FILTER as libc::c_long,
                0u64,
                &mut prog as *mut sock_fprog,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
