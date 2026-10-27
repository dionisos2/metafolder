//! The system calls no safe wrapper covers (doc "Unsafe code"): fanotify, file
//! handles, a filesystem's fsid, a socket peer's credentials. Each is a safe
//! function over one unsafe block; the rest of the crate calls these, or
//! rustix. A descriptor is taken as a `RawFd`: a wrong one is an error the
//! kernel returns (EBADF), never memory unsafety.

use std::ffi::CStr;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// `fanotify_init(flags, event_f_flags)`.
pub fn fanotify_init(flags: u32, event_f_flags: u32) -> io::Result<OwnedFd> {
    // SAFETY: plain integer arguments.
    let fd = unsafe { libc::fanotify_init(flags, event_f_flags) };
    owned(fd)
}

/// `fanotify_mark(group, flags, mask, dirfd, path)`; no `path` is a null
/// pathname (the mark is then on `dirfd` itself, or a flush).
pub fn fanotify_mark(
    group: RawFd,
    flags: u32,
    mask: u64,
    dirfd: RawFd,
    path: Option<&CStr>,
) -> io::Result<()> {
    let path = path.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: `path` is null or a NUL-terminated string that outlives the call.
    check(unsafe { libc::fanotify_mark(group, flags, mask, dirfd, path) })
}

/// `struct file_handle` is a header (u32 + i32) followed by the opaque bytes —
/// laid out by hand because the uapi struct ends in a flexible array.
const HANDLE_HEADER: usize = 8;

/// `open_by_handle_at(mount_fd, handle, flags)`, close-on-exec.
pub fn open_by_handle_at(
    mount_fd: RawFd,
    handle_type: i32,
    bytes: &[u8],
    flags: libc::c_int,
) -> io::Result<OwnedFd> {
    let mut buf = vec![0u8; HANDLE_HEADER + bytes.len()];
    buf[0..4].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
    buf[4..8].copy_from_slice(&handle_type.to_ne_bytes());
    buf[HANDLE_HEADER..].copy_from_slice(bytes);
    let fh = buf.as_mut_ptr() as *mut libc::file_handle;
    // SAFETY: `buf` is a `file_handle` whose header gives the exact length of
    // the bytes that follow it, and it outlives the call.
    let fd = unsafe { libc::open_by_handle_at(mount_fd, fh, flags | libc::O_CLOEXEC) };
    owned(fd)
}

/// `name_to_handle_at(AT_FDCWD, path, …, 0)`: the handle's type and bytes.
pub fn name_to_handle_at(path: &CStr) -> io::Result<(i32, Vec<u8>)> {
    let max = libc::MAX_HANDLE_SZ as usize;
    let mut buf = vec![0u8; HANDLE_HEADER + max];
    buf[0..4].copy_from_slice(&(max as u32).to_ne_bytes());
    let fh = buf.as_mut_ptr() as *mut libc::file_handle;
    let mut mount_id: libc::c_int = 0;
    // SAFETY: `buf` is a `file_handle` announcing the room that follows its
    // header (MAX_HANDLE_SZ), which the kernel writes no further than; `path`
    // is NUL-terminated; all outlive the call.
    check(unsafe { libc::name_to_handle_at(libc::AT_FDCWD, path.as_ptr(), fh, &mut mount_id, 0) })?;
    let len = (u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize).min(max);
    let handle_type = i32::from_ne_bytes(buf[4..8].try_into().unwrap());
    Ok((handle_type, buf[HANDLE_HEADER..HANDLE_HEADER + len].to_vec()))
}

/// The `fsid` of the filesystem holding `path` (`statfs` reports the same
/// `__kernel_fsid_t` the events carry). Not through rustix, whose `Fsid`
/// keeps its two ints private.
pub fn statfs_fsid(path: &CStr) -> io::Result<[i32; 2]> {
    // SAFETY: `statfs` is plain old data, for which all zeroes is valid.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a struct the kernel fills, both live.
    check(unsafe { libc::statfs(path.as_ptr(), &mut st) })?;
    Ok(fsid(&st))
}

/// [`statfs_fsid`] of an open descriptor.
pub fn fstatfs_fsid(fd: RawFd) -> io::Result<[i32; 2]> {
    // SAFETY: as in `statfs_fsid`.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a struct the kernel fills, live.
    check(unsafe { libc::fstatfs(fd, &mut st) })?;
    Ok(fsid(&st))
}

fn fsid(st: &libc::statfs) -> [i32; 2] {
    // `fsid_t` hides its two ints; the events carry them plainly.
    const _: () = assert!(size_of::<libc::fsid_t>() == size_of::<[i32; 2]>());
    // SAFETY: `fsid_t` is two `int`s (`__val`), the same size (checked above)
    // and layout as `[i32; 2]`.
    unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(st.f_fsid) }
}

/// `SO_PEERCRED` of a socket: pid, uid, gid as they were at `connect`. Not
/// through rustix, whose `Pid` cannot hold the 0 a peer in another pid
/// namespace has.
pub fn peer_cred(fd: RawFd) -> io::Result<(i32, u32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live and `len` is `cred`'s size, which the
    // kernel writes no further than.
    check(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    })?;
    Ok((cred.pid, cred.uid, cred.gid))
}

/// `SO_PEERGROUPS` into `groups`: the number of groups the peer has, which may
/// be more than `groups` holds (ERANGE: ask again with that much room).
pub fn peer_groups(fd: RawFd, groups: &mut [u32]) -> (io::Result<()>, usize) {
    let mut len = size_of_val(groups) as libc::socklen_t;
    // SAFETY: `groups` is live and `len` its size in bytes, which the kernel
    // writes no further than.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERGROUPS,
            groups.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (check(rc), len as usize / size_of::<u32>())
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn owned(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just returned, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
