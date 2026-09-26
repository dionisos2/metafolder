//! The syscall filter of the media-helper sandbox (`crate::sandbox`).
//!
//! `bwrap` bounds what a compromised decoder can *reach* on the filesystem and
//! the network; it does not bound what it can ask the *kernel*. A sandboxed
//! process can still create a user namespace (and with it every capability
//! inside it — the opening move of most kernel privilege escalations), or
//! reach interfaces a media decoder never needs and that carry a long CVE
//! record: `io_uring`, `bpf`, `keyctl`, `userfaultfd`, `perf_event_open`…
//!
//! This is a deny-list, in the spirit of flatpak's: everything a decoder does
//! (file I/O, memory, threads, signals) stays allowed, and the handful of
//! interfaces listed here answer `EPERM`. Namespaces are refused by *flags*:
//! `clone` is checked for `CLONE_NEW*`, and `clone3` — whose flags sit behind a
//! pointer a filter cannot read — answers `ENOSYS`, so the C library falls back
//! to `clone`. (`bwrap --disable-userns` would do part of this, but it writes
//! `/proc/sys/user/max_user_namespaces`, which is read-only in some containers.)
//!
//! Only on the architectures listed: elsewhere [`program`] is `None` and the
//! rest of the sandbox stands alone. A syscall from a foreign ABI (i386 or x32
//! on x86-64) kills the process — the numbers would not mean what the list
//! says.

/// The classic-BPF instruction (`struct sock_filter`).
#[derive(Clone, Copy)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD (0) | BPF_W (0) | BPF_ABS (0x20)
const BPF_JEQ_K: u16 = 0x05 | 0x10; // BPF_JMP | BPF_JEQ | BPF_K
const BPF_JGE_K: u16 = 0x05 | 0x30; // BPF_JMP | BPF_JGE | BPF_K
const BPF_JSET_K: u16 = 0x05 | 0x40; // BPF_JMP | BPF_JSET | BPF_K
const BPF_RET_K: u16 = 0x06;

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;

/// `struct seccomp_data` offsets: `nr`, `arch`, then (after `ip`) `args[0]`,
/// whose low 32 bits come first on the little-endian targets we build for.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0_LOW: u32 = 16;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const AUDIT_ARCH: Option<u32> = Some(0xc000_003e);
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const AUDIT_ARCH: Option<u32> = Some(0xc000_00b7);
#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
const AUDIT_ARCH: Option<u32> = None;

/// Every namespace-creating clone flag.
#[cfg(target_os = "linux")]
const CLONE_NEW_ANY: u32 = (libc::CLONE_NEWUSER
    | libc::CLONE_NEWNS
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWCGROUP
    | 0x80) as u32; // CLONE_NEWTIME

/// Refused with `EPERM`: none is needed to decode an image, a video or a PDF.
#[cfg(target_os = "linux")]
const DENIED: &[libc::c_long] = &[
    // Namespaces and the mount table.
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_open_tree,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    // The large, bug-prone kernel interfaces.
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    // Other processes and the kernel itself.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    libc::SYS_fanotify_init,
    libc::SYS_syslog,
    libc::SYS_acct,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_reboot,
    libc::SYS_quotactl,
];

/// The filter as the bytes `bwrap --add-seccomp-fd` reads (an array of
/// `struct sock_filter`), or `None` on an architecture without a table here.
#[cfg(target_os = "linux")]
pub fn program() -> Option<Vec<u8>> {
    let arch = AUDIT_ARCH?;
    let stmt = |code, k| Insn { code, jt: 0, jf: 0, k };
    let jump = |code, k, jt, jf| Insn { code, jt, jf, k };
    let errno = |e: i32| stmt(BPF_RET_K, RET_ERRNO | (e as u32 & 0xffff));

    let mut p = vec![
        stmt(BPF_LD_W_ABS, OFF_ARCH),
        jump(BPF_JEQ_K, arch, 1, 0),
        stmt(BPF_RET_K, RET_KILL_PROCESS),
        stmt(BPF_LD_W_ABS, OFF_NR),
    ];
    // x32 shares the x86-64 audit arch, with numbers offset by this bit.
    if cfg!(target_arch = "x86_64") {
        p.push(jump(BPF_JGE_K, 0x4000_0000, 0, 1));
        p.push(stmt(BPF_RET_K, RET_KILL_PROCESS));
    }
    for &nr in DENIED {
        p.push(jump(BPF_JEQ_K, nr as u32, 0, 1));
        p.push(errno(libc::EPERM));
    }
    p.push(jump(BPF_JEQ_K, libc::SYS_clone3 as u32, 0, 1));
    p.push(errno(libc::ENOSYS));
    // clone: allowed unless it asks for a namespace (the accumulator is
    // reloaded with the flags only on this branch).
    p.push(jump(BPF_JEQ_K, libc::SYS_clone as u32, 0, 3));
    p.push(stmt(BPF_LD_W_ABS, OFF_ARG0_LOW));
    p.push(jump(BPF_JSET_K, CLONE_NEW_ANY, 0, 1));
    p.push(errno(libc::EPERM));
    p.push(stmt(BPF_RET_K, RET_ALLOW));

    let mut bytes = Vec::with_capacity(p.len() * 8);
    for insn in p {
        bytes.extend_from_slice(&insn.code.to_ne_bytes());
        bytes.push(insn.jt);
        bytes.push(insn.jf);
        bytes.extend_from_slice(&insn.k.to_ne_bytes());
    }
    Some(bytes)
}

#[cfg(not(target_os = "linux"))]
pub fn program() -> Option<Vec<u8>> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn test_the_program_is_whole_instructions_and_fits_the_kernel_limit() {
        let Some(bytes) = program() else { return };
        assert_eq!(bytes.len() % 8, 0);
        // BPF_MAXINSNS.
        assert!(bytes.len() / 8 <= 4096);
    }

    #[test]
    fn test_the_last_instruction_allows() {
        let Some(bytes) = program() else { return };
        let last = &bytes[bytes.len() - 8..];
        assert_eq!(u16::from_ne_bytes([last[0], last[1]]), BPF_RET_K);
        assert_eq!(u32::from_ne_bytes([last[4], last[5], last[6], last[7]]), RET_ALLOW);
    }
}
