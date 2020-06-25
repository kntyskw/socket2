// Copyright 2015 The Rust Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::net::{self, Ipv4Addr, Ipv6Addr};
#[cfg(feature = "all")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
#[cfg(feature = "all")]
use std::path::Path;
#[cfg(feature = "all")]
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{cmp, fmt, io, mem};

use libc::{self, c_void, ssize_t};

use crate::{Domain, Type};

pub use libc::c_int;

// Used in `Domain`.
pub(crate) use libc::{AF_INET, AF_INET6};
// Used in `Type`.
pub(crate) use libc::{SOCK_DGRAM, SOCK_STREAM};
#[cfg(all(feature = "all", not(target_os = "redox")))]
pub(crate) use libc::{SOCK_RAW, SOCK_SEQPACKET};
// Used in `Protocol`.
pub(crate) use libc::{IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_TCP, IPPROTO_UDP};
// Used in `SockAddr`.
pub(crate) use libc::{
    sa_family_t, sockaddr, sockaddr_in, sockaddr_in6, sockaddr_storage, socklen_t,
};

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "dragonfly", target_os = "freebsd",
                 target_os = "ios", target_os = "macos",
                 target_os = "openbsd", target_os = "netbsd",
                 target_os = "solaris", target_os = "illumos",
                 target_os = "haiku"))] {
        use libc::IPV6_JOIN_GROUP as IPV6_ADD_MEMBERSHIP;
        use libc::IPV6_LEAVE_GROUP as IPV6_DROP_MEMBERSHIP;
    } else {
        use libc::IPV6_ADD_MEMBERSHIP;
        use libc::IPV6_DROP_MEMBERSHIP;
    }
}

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "macos", target_os = "ios"))] {
        use libc::TCP_KEEPALIVE as KEEPALIVE_OPTION;
    } else if #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "haiku"))] {
        use libc::SO_KEEPALIVE as KEEPALIVE_OPTION;
    } else {
        use libc::TCP_KEEPIDLE as KEEPALIVE_OPTION;
    }
}

use crate::SockAddr;

/// Helper macro to execute a system call that returns an `io::Result`.
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// Unix only API.
impl Domain {
    /// Domain for Unix socket communication, corresponding to `AF_UNIX`.
    pub const UNIX: Domain = Domain(libc::AF_UNIX);

    /// Domain for low-level packet interface, corresponding to `AF_PACKET`.
    ///
    /// # Notes
    ///
    /// This function is only available on Linux.
    #[cfg(all(feature = "all", target_os = "linux"))]
    pub const PACKET: Domain = Domain(libc::AF_PACKET);
}

impl_debug!(
    Domain,
    libc::AF_INET,
    libc::AF_INET6,
    libc::AF_UNIX,
    #[cfg(target_os = "linux")]
    libc::AF_PACKET,
    #[cfg(not(target_os = "redox"))]
    libc::AF_UNSPEC, // = 0.
);

/// Unix only API.
impl Type {
    /// Set `SOCK_NONBLOCK` on the `Type`.
    ///
    /// # Notes
    ///
    /// This function is only available on Android, DragonFlyBSD, FreeBSD,
    /// Linux, NetBSD and OpenBSD.
    #[cfg(all(
        feature = "all",
        any(
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "linux",
            target_os = "netbsd",
            target_os = "openbsd"
        )
    ))]
    pub const fn non_blocking(self) -> Type {
        Type(self.0 | libc::SOCK_NONBLOCK)
    }

    /// Set `SOCK_CLOEXEC` on the `Type`.
    ///
    /// # Notes
    ///
    /// This function is only available on Android, DragonFlyBSD, FreeBSD,
    /// Linux, NetBSD and OpenBSD.
    #[cfg(all(
        feature = "all",
        any(
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "linux",
            target_os = "netbsd",
            target_os = "openbsd"
        )
    ))]
    pub const fn cloexec(self) -> Type {
        Type(self.0 | libc::SOCK_CLOEXEC)
    }
}

impl_debug!(
    crate::Type,
    libc::SOCK_STREAM,
    libc::SOCK_DGRAM,
    #[cfg(not(target_os = "redox"))]
    libc::SOCK_RAW,
    #[cfg(not(any(target_os = "redox", target_os = "haiku")))]
    libc::SOCK_RDM,
    #[cfg(not(target_os = "redox"))]
    libc::SOCK_SEQPACKET,
    /* TODO: add these optional bit OR-ed flags:
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    libc::SOCK_NONBLOCK,
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    libc::SOCK_CLOEXEC,
    */
);

impl_debug!(
    crate::Protocol,
    libc::IPPROTO_ICMP,
    libc::IPPROTO_ICMPV6,
    libc::IPPROTO_TCP,
    libc::IPPROTO_UDP,
);

/// Unix only API.
impl SockAddr {
    /// Constructs a `SockAddr` with the family `AF_UNIX` and the provided path.
    ///
    /// This function is only available on Unix.
    ///
    /// # Failure
    ///
    /// Returns an error if the path is longer than `SUN_LEN`.
    #[cfg(feature = "all")]
    pub fn unix<P>(path: P) -> io::Result<SockAddr>
    where
        P: AsRef<Path>,
    {
        // Safety: zeroed `sockaddr_un` is valid.
        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };

        let bytes = path.as_ref().as_os_str().as_bytes();
        if bytes.len() >= addr.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must be shorter than SUN_LEN",
            ));
        }

        addr.sun_family = libc::AF_UNIX as sa_family_t;
        // Safety: `bytes` and `addr.sun_path` are not overlapping and `bytes`
        // points to valid memory.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                addr.sun_path.as_mut_ptr() as *mut u8,
                bytes.len(),
            )
        };
        // Zeroed memory above, so the path is already null terminated.

        let base = &addr as *const _ as usize;
        let path = &addr.sun_path as *const _ as usize;
        let sun_path_offset = path - base;
        let mut len = sun_path_offset + bytes.len();
        match bytes.first() {
            Some(&0) | None => {}
            Some(_) => len += 1,
        };
        Ok(unsafe { SockAddr::from_raw_parts(&addr as *const _ as *const _, len as socklen_t) })
    }
}

pub struct Socket {
    fd: c_int,
}

impl Socket {
    pub fn new(family: c_int, ty: c_int, protocol: c_int) -> io::Result<Socket> {
        // On linux we first attempt to pass the SOCK_CLOEXEC flag to atomically
        // create the socket and set it as CLOEXEC. Support for this option,
        // however, was added in 2.6.27, and we still support 2.6.18 as a
        // kernel, so if the returned error is EINVAL we fallthrough to the
        // fallback.
        #[cfg(target_os = "linux")]
        {
            match syscall!(socket(family, ty | libc::SOCK_CLOEXEC, protocol)) {
                Ok(fd) => return unsafe { Ok(Socket::from_raw_fd(fd)) },
                Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => {}
                Err(e) => return Err(e),
            }
        }

        let fd = syscall!(socket(family, ty, protocol))?;
        let fd = unsafe { Socket::from_raw_fd(fd) };
        set_cloexec(fd.as_raw_fd())?;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        unsafe {
            fd.setsockopt(libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1i32)?;
        }
        Ok(fd)
    }

    pub fn pair(family: c_int, ty: c_int, protocol: c_int) -> io::Result<(Socket, Socket)> {
        let mut fds = [0, 0];
        syscall!(socketpair(family, ty, protocol, fds.as_mut_ptr()))?;
        let fds = unsafe { (Socket::from_raw_fd(fds[0]), Socket::from_raw_fd(fds[1])) };
        set_cloexec(fds.0.as_raw_fd())?;
        set_cloexec(fds.1.as_raw_fd())?;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        unsafe {
            fds.0
                .setsockopt(libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1i32)?;
            fds.1
                .setsockopt(libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1i32)?;
        }
        Ok(fds)
    }

    pub fn bind(&self, addr: &SockAddr) -> io::Result<()> {
        syscall!(bind(self.fd, addr.as_ptr(), addr.len() as _)).map(|_| ())
    }

    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        syscall!(listen(self.fd, backlog)).map(|_| ())
    }

    pub fn connect(&self, addr: &SockAddr) -> io::Result<()> {
        syscall!(connect(self.fd, addr.as_ptr(), addr.len())).map(|_| ())
    }

    pub fn connect_timeout(&self, addr: &SockAddr, timeout: Duration) -> io::Result<()> {
        self.set_nonblocking(true)?;
        let r = self.connect(addr);
        self.set_nonblocking(false)?;

        match r {
            Ok(()) => return Ok(()),
            // there's no io::ErrorKind conversion registered for EINPROGRESS :(
            Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(e) => return Err(e),
        }

        let mut pollfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };

        if timeout.as_secs() == 0 && timeout.subsec_nanos() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot set a 0 duration timeout",
            ));
        }

        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection timed out",
                ));
            }

            let timeout = timeout - elapsed;
            let mut timeout = timeout
                .as_secs()
                .saturating_mul(1_000)
                .saturating_add(timeout.subsec_nanos() as u64 / 1_000_000);
            if timeout == 0 {
                timeout = 1;
            }

            let timeout = cmp::min(timeout, c_int::max_value() as u64) as c_int;

            match unsafe { libc::poll(&mut pollfd, 1, timeout) } {
                -1 => {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "connection timed out",
                    ))
                }
                _ => {
                    // linux returns POLLOUT|POLLERR|POLLHUP for refused connections (!), so look
                    // for POLLHUP rather than read readiness
                    if pollfd.revents & libc::POLLHUP != 0 {
                        let e = self.take_error()?.unwrap_or_else(|| {
                            io::Error::new(io::ErrorKind::Other, "no error set after POLLHUP")
                        });
                        return Err(e);
                    }
                    return Ok(());
                }
            }
        }
    }

    pub fn local_addr(&self) -> io::Result<SockAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = mem::size_of_val(&storage) as libc::socklen_t;
        syscall!(getsockname(
            self.fd,
            &mut storage as *mut _ as *mut _,
            &mut len,
        ))?;
        Ok(unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, len) })
    }

    pub fn peer_addr(&self) -> io::Result<SockAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = mem::size_of_val(&storage) as libc::socklen_t;
        syscall!(getpeername(
            self.fd,
            &mut storage as *mut _ as *mut _,
            &mut len,
        ))?;
        Ok(unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, len) })
    }

    pub fn try_clone(&self) -> io::Result<Socket> {
        // implementation lifted from libstd
        #[cfg(any(target_os = "android", target_os = "haiku"))]
        use libc::F_DUPFD as F_DUPFD_CLOEXEC;
        #[cfg(not(any(target_os = "android", target_os = "haiku")))]
        use libc::F_DUPFD_CLOEXEC;

        static CLOEXEC_FAILED: AtomicBool = AtomicBool::new(false);
        if !CLOEXEC_FAILED.load(Ordering::Relaxed) {
            match syscall!(fcntl(self.fd, F_DUPFD_CLOEXEC, 0)) {
                Ok(fd) => {
                    let fd = unsafe { Socket::from_raw_fd(fd) };
                    if cfg!(target_os = "linux") {
                        set_cloexec(fd.as_raw_fd())?;
                    }
                    return Ok(fd);
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EINVAL) => {
                    CLOEXEC_FAILED.store(true, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        let fd = syscall!(fcntl(self.fd, libc::F_DUPFD, 0))?;
        let fd = unsafe { Socket::from_raw_fd(fd) };
        set_cloexec(fd.as_raw_fd())?;
        Ok(fd)
    }

    #[allow(unused_mut)]
    pub fn accept(&self) -> io::Result<(Socket, SockAddr)> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = mem::size_of_val(&storage) as socklen_t;

        let mut socket = None;
        #[cfg(target_os = "linux")]
        {
            let res = syscall!(accept4(
                self.fd,
                &mut storage as *mut _ as *mut _,
                &mut len,
                libc::SOCK_CLOEXEC,
            ));
            match res {
                Ok(fd) => socket = Some(Socket { fd: fd }),
                Err(ref e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
                Err(e) => return Err(e),
            }
        }

        let socket = match socket {
            Some(socket) => socket,
            None => {
                let fd = syscall!(accept(self.fd, &mut storage as *mut _ as *mut _, &mut len))?;
                let fd = unsafe { Socket::from_raw_fd(fd) };
                set_cloexec(fd.as_raw_fd())?;
                fd
            }
        };
        let addr = unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, len) };
        Ok((socket, addr))
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_ERROR)?;
            if raw == 0 {
                Ok(None)
            } else {
                Ok(Some(io::Error::from_raw_os_error(raw as i32)))
            }
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let previous = syscall!(fcntl(self.fd, libc::F_GETFL))?;
        let new = if nonblocking {
            previous | libc::O_NONBLOCK
        } else {
            previous & !libc::O_NONBLOCK
        };
        if new != previous {
            syscall!(fcntl(self.fd, libc::F_SETFL, new))?;
        }
        Ok(())
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Write => libc::SHUT_WR,
            Shutdown::Read => libc::SHUT_RD,
            Shutdown::Both => libc::SHUT_RDWR,
        };
        syscall!(shutdown(self.fd, how))?;
        Ok(())
    }

    pub fn recv(&self, buf: &mut [u8], flags: c_int) -> io::Result<usize> {
        let n = syscall!(recv(
            self.fd,
            buf.as_mut_ptr() as *mut c_void,
            cmp::min(buf.len(), max_len()),
            flags,
        ))?;
        Ok(n as usize)
    }

    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = syscall!(recv(
            self.fd,
            buf.as_mut_ptr() as *mut c_void,
            cmp::min(buf.len(), max_len()),
            libc::MSG_PEEK,
        ))?;
        Ok(n as usize)
    }

    pub fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SockAddr)> {
        self.recv_from(buf, libc::MSG_PEEK)
    }

    pub fn recv_from(&self, buf: &mut [u8], flags: c_int) -> io::Result<(usize, SockAddr)> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut addrlen = mem::size_of_val(&storage) as socklen_t;

        let n = syscall!(recvfrom(
            self.fd,
            buf.as_mut_ptr() as *mut c_void,
            cmp::min(buf.len(), max_len()),
            flags,
            &mut storage as *mut _ as *mut _,
            &mut addrlen,
        ))?;
        let addr = unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, addrlen) };
        Ok((n as usize, addr))
    }

    pub fn send(&self, buf: &[u8], flags: c_int) -> io::Result<usize> {
        let n = syscall!(send(
            self.fd,
            buf.as_ptr() as *const c_void,
            cmp::min(buf.len(), max_len()),
            flags,
        ))?;
        Ok(n as usize)
    }

    pub fn send_to(&self, buf: &[u8], flags: c_int, addr: &SockAddr) -> io::Result<usize> {
        let n = syscall!(sendto(
            self.fd,
            buf.as_ptr() as *const c_void,
            cmp::min(buf.len(), max_len()),
            flags,
            addr.as_ptr(),
            addr.len(),
        ))?;
        Ok(n as usize)
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IP, libc::IP_TTL)?;
            Ok(raw as u32)
        }
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        unsafe { self.setsockopt(libc::IPPROTO_IP, libc::IP_TTL, ttl as c_int) }
    }

    pub fn unicast_hops_v6(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IPV6, libc::IPV6_UNICAST_HOPS)?;
            Ok(raw as u32)
        }
    }

    pub fn set_unicast_hops_v6(&self, hops: u32) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                libc::IPPROTO_IPV6 as c_int,
                libc::IPV6_UNICAST_HOPS,
                hops as c_int,
            )
        }
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)?;
            Ok(raw != 0)
        }
    }

    pub fn set_only_v6(&self, only_v6: bool) -> io::Result<()> {
        unsafe { self.setsockopt(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, only_v6 as c_int) }
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        unsafe {
            Ok(timeval2dur(
                self.getsockopt(libc::SOL_SOCKET, libc::SO_RCVTIMEO)?,
            ))
        }
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_RCVTIMEO, dur2timeval(dur)?) }
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        unsafe {
            Ok(timeval2dur(
                self.getsockopt(libc::SOL_SOCKET, libc::SO_SNDTIMEO)?,
            ))
        }
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_SNDTIMEO, dur2timeval(dur)?) }
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_TCP, libc::TCP_NODELAY)?;
            Ok(raw != 0)
        }
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        unsafe { self.setsockopt(libc::IPPROTO_TCP, libc::TCP_NODELAY, nodelay as c_int) }
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_BROADCAST)?;
            Ok(raw != 0)
        }
    }

    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_BROADCAST, broadcast as c_int) }
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IP, libc::IP_MULTICAST_LOOP)?;
            Ok(raw != 0)
        }
    }

    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_LOOP,
                multicast_loop_v4 as c_int,
            )
        }
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IP, libc::IP_MULTICAST_TTL)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_TTL,
                multicast_ttl_v4 as c_int,
            )
        }
    }

    pub fn multicast_hops_v6(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_HOPS)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_hops_v6(&self, hops: u32) -> io::Result<()> {
        unsafe { self.setsockopt(libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_HOPS, hops as c_int) }
    }

    pub fn multicast_if_v4(&self) -> io::Result<Ipv4Addr> {
        unsafe {
            let imr_interface: libc::in_addr =
                self.getsockopt(libc::IPPROTO_IP, libc::IP_MULTICAST_IF)?;
            Ok(from_s_addr(imr_interface.s_addr))
        }
    }

    pub fn set_multicast_if_v4(&self, interface: &Ipv4Addr) -> io::Result<()> {
        let interface = to_s_addr(interface);
        let imr_interface = libc::in_addr { s_addr: interface };

        unsafe { self.setsockopt(libc::IPPROTO_IP, libc::IP_MULTICAST_IF, imr_interface) }
    }

    pub fn multicast_if_v6(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_IF)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_if_v6(&self, interface: u32) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                libc::IPPROTO_IPV6,
                libc::IPV6_MULTICAST_IF,
                interface as c_int,
            )
        }
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_LOOP)?;
            Ok(raw != 0)
        }
    }

    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                libc::IPPROTO_IPV6,
                libc::IPV6_MULTICAST_LOOP,
                multicast_loop_v6 as c_int,
            )
        }
    }

    pub fn join_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        let multiaddr = to_s_addr(multiaddr);
        let interface = to_s_addr(interface);
        let mreq = libc::ip_mreq {
            imr_multiaddr: libc::in_addr { s_addr: multiaddr },
            imr_interface: libc::in_addr { s_addr: interface },
        };
        unsafe { self.setsockopt(libc::IPPROTO_IP, libc::IP_ADD_MEMBERSHIP, mreq) }
    }

    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        let multiaddr = to_in6_addr(multiaddr);
        let mreq = libc::ipv6_mreq {
            ipv6mr_multiaddr: multiaddr,
            ipv6mr_interface: to_ipv6mr_interface(interface),
        };
        unsafe { self.setsockopt(libc::IPPROTO_IPV6, IPV6_ADD_MEMBERSHIP, mreq) }
    }

    pub fn leave_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        let multiaddr = to_s_addr(multiaddr);
        let interface = to_s_addr(interface);
        let mreq = libc::ip_mreq {
            imr_multiaddr: libc::in_addr { s_addr: multiaddr },
            imr_interface: libc::in_addr { s_addr: interface },
        };
        unsafe { self.setsockopt(libc::IPPROTO_IP, libc::IP_DROP_MEMBERSHIP, mreq) }
    }

    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        let multiaddr = to_in6_addr(multiaddr);
        let mreq = libc::ipv6_mreq {
            ipv6mr_multiaddr: multiaddr,
            ipv6mr_interface: to_ipv6mr_interface(interface),
        };
        unsafe { self.setsockopt(libc::IPPROTO_IPV6, IPV6_DROP_MEMBERSHIP, mreq) }
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsafe {
            Ok(linger2dur(
                self.getsockopt(libc::SOL_SOCKET, libc::SO_LINGER)?,
            ))
        }
    }

    pub fn set_linger(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_LINGER, dur2linger(dur)) }
    }

    pub fn set_reuse_address(&self, reuse: bool) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_REUSEADDR, reuse as c_int) }
    }

    pub fn reuse_address(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
            Ok(raw != 0)
        }
    }

    pub fn recv_buffer_size(&self) -> io::Result<usize> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_RCVBUF)?;
            Ok(raw as usize)
        }
    }

    pub fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
        unsafe {
            // TODO: casting usize to a c_int should be a checked cast
            self.setsockopt(libc::SOL_SOCKET, libc::SO_RCVBUF, size as c_int)
        }
    }

    pub fn send_buffer_size(&self) -> io::Result<usize> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_SNDBUF)?;
            Ok(raw as usize)
        }
    }

    pub fn set_send_buffer_size(&self, size: usize) -> io::Result<()> {
        unsafe {
            // TODO: casting usize to a c_int should be a checked cast
            self.setsockopt(libc::SOL_SOCKET, libc::SO_SNDBUF, size as c_int)
        }
    }

    pub fn keepalive(&self) -> io::Result<Option<Duration>> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_KEEPALIVE)?;
            if raw == 0 {
                return Ok(None);
            }
            let secs: c_int = self.getsockopt(libc::IPPROTO_TCP, KEEPALIVE_OPTION)?;
            Ok(Some(Duration::new(secs as u64, 0)))
        }
    }

    pub fn set_keepalive(&self, keepalive: Option<Duration>) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                keepalive.is_some() as c_int,
            )?;
            if let Some(dur) = keepalive {
                // TODO: checked cast here
                self.setsockopt(libc::IPPROTO_TCP, KEEPALIVE_OPTION, dur.as_secs() as c_int)?;
            }
            Ok(())
        }
    }

    #[cfg(not(any(target_os = "solaris", target_os = "illumos")))]
    pub fn reuse_port(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_REUSEPORT)?;
            Ok(raw != 0)
        }
    }

    #[cfg(not(any(target_os = "solaris", target_os = "illumos")))]
    pub fn set_reuse_port(&self, reuse: bool) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_REUSEPORT, reuse as c_int) }
    }

    #[cfg(all(feature = "all", not(target_os = "redox")))]
    pub fn out_of_band_inline(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(libc::SOL_SOCKET, libc::SO_OOBINLINE)?;
            Ok(raw != 0)
        }
    }

    #[cfg(all(feature = "all", not(target_os = "redox")))]
    pub fn set_out_of_band_inline(&self, oob_inline: bool) -> io::Result<()> {
        unsafe { self.setsockopt(libc::SOL_SOCKET, libc::SO_OOBINLINE, oob_inline as c_int) }
    }

    unsafe fn setsockopt<T>(&self, opt: c_int, val: c_int, payload: T) -> io::Result<()>
    where
        T: Copy,
    {
        let payload = &payload as *const T as *const c_void;
        syscall!(setsockopt(
            self.fd,
            opt,
            val,
            payload,
            mem::size_of::<T>() as libc::socklen_t,
        ))?;
        Ok(())
    }

    unsafe fn getsockopt<T: Copy>(&self, opt: c_int, val: c_int) -> io::Result<T> {
        let mut slot: T = mem::zeroed();
        let mut len = mem::size_of::<T>() as libc::socklen_t;
        syscall!(getsockopt(
            self.fd,
            opt,
            val,
            &mut slot as *mut _ as *mut _,
            &mut len,
        ))?;
        assert_eq!(len as usize, mem::size_of::<T>());
        Ok(slot)
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        <&Socket>::read(&mut &*self, buf)
    }
}

impl<'a> Read for &'a Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = syscall!(read(
            self.fd,
            buf.as_mut_ptr() as *mut c_void,
            cmp::min(buf.len(), max_len()),
        ))?;
        Ok(n as usize)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        <&Socket>::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        <&Socket>::flush(&mut &*self)
    }
}

impl<'a> Write for &'a Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf, 0)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("Socket");
        f.field("fd", &self.fd);
        if let Ok(addr) = self.local_addr() {
            f.field("local_addr", &addr);
        }
        if let Ok(addr) = self.peer_addr() {
            f.field("peer_addr", &addr);
        }
        f.finish()
    }
}

impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> c_int {
        self.fd
    }
}

impl IntoRawFd for Socket {
    fn into_raw_fd(self) -> c_int {
        let fd = self.fd;
        mem::forget(self);
        return fd;
    }
}

impl FromRawFd for Socket {
    unsafe fn from_raw_fd(fd: c_int) -> Socket {
        Socket { fd: fd }
    }
}

impl AsRawFd for crate::Socket {
    fn as_raw_fd(&self) -> c_int {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for crate::Socket {
    fn into_raw_fd(self) -> c_int {
        self.inner.into_raw_fd()
    }
}

impl FromRawFd for crate::Socket {
    unsafe fn from_raw_fd(fd: c_int) -> crate::Socket {
        crate::Socket {
            inner: Socket::from_raw_fd(fd),
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::close(self.fd);
        }
    }
}

impl From<Socket> for net::TcpStream {
    fn from(socket: Socket) -> net::TcpStream {
        unsafe { net::TcpStream::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<Socket> for net::TcpListener {
    fn from(socket: Socket) -> net::TcpListener {
        unsafe { net::TcpListener::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<Socket> for net::UdpSocket {
    fn from(socket: Socket) -> net::UdpSocket {
        unsafe { net::UdpSocket::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<Socket> for UnixStream {
    fn from(socket: Socket) -> UnixStream {
        unsafe { UnixStream::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<Socket> for UnixListener {
    fn from(socket: Socket) -> UnixListener {
        unsafe { UnixListener::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<Socket> for UnixDatagram {
    fn from(socket: Socket) -> UnixDatagram {
        unsafe { UnixDatagram::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<net::TcpStream> for Socket {
    fn from(socket: net::TcpStream) -> Socket {
        unsafe { Socket::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<net::TcpListener> for Socket {
    fn from(socket: net::TcpListener) -> Socket {
        unsafe { Socket::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<net::UdpSocket> for Socket {
    fn from(socket: net::UdpSocket) -> Socket {
        unsafe { Socket::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<UnixStream> for Socket {
    fn from(socket: UnixStream) -> Socket {
        unsafe { Socket::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<UnixListener> for Socket {
    fn from(socket: UnixListener) -> Socket {
        unsafe { Socket::from_raw_fd(socket.into_raw_fd()) }
    }
}

impl From<UnixDatagram> for Socket {
    fn from(socket: UnixDatagram) -> Socket {
        unsafe { Socket::from_raw_fd(socket.into_raw_fd()) }
    }
}

fn max_len() -> usize {
    // The maximum read limit on most posix-like systems is `SSIZE_MAX`,
    // with the man page quoting that if the count of bytes to read is
    // greater than `SSIZE_MAX` the result is "unspecified".
    //
    // On macOS, however, apparently the 64-bit libc is either buggy or
    // intentionally showing odd behavior by rejecting any read with a size
    // larger than or equal to INT_MAX. To handle both of these the read
    // size is capped on both platforms.
    if cfg!(target_os = "macos") {
        <c_int>::max_value() as usize - 1
    } else {
        <ssize_t>::max_value() as usize
    }
}

fn set_cloexec(fd: c_int) -> io::Result<()> {
    let previous = syscall!(fcntl(fd, libc::F_GETFD))?;
    let new = previous | libc::FD_CLOEXEC;
    if new != previous {
        syscall!(fcntl(fd, libc::F_SETFD, new))?;
    }
    Ok(())
}

fn dur2timeval(dur: Option<Duration>) -> io::Result<libc::timeval> {
    match dur {
        Some(dur) => {
            if dur.as_secs() == 0 && dur.subsec_nanos() == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot set a 0 duration timeout",
                ));
            }

            let secs = if dur.as_secs() > libc::time_t::max_value() as u64 {
                libc::time_t::max_value()
            } else {
                dur.as_secs() as libc::time_t
            };
            let mut timeout = libc::timeval {
                tv_sec: secs,
                tv_usec: (dur.subsec_nanos() / 1000) as libc::suseconds_t,
            };
            if timeout.tv_sec == 0 && timeout.tv_usec == 0 {
                timeout.tv_usec = 1;
            }
            Ok(timeout)
        }
        None => Ok(libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        }),
    }
}

fn timeval2dur(raw: libc::timeval) -> Option<Duration> {
    if raw.tv_sec == 0 && raw.tv_usec == 0 {
        None
    } else {
        let sec = raw.tv_sec as u64;
        let nsec = (raw.tv_usec as u32) * 1000;
        Some(Duration::new(sec, nsec))
    }
}

fn to_s_addr(addr: &Ipv4Addr) -> libc::in_addr_t {
    let octets = addr.octets();
    u32::from_ne_bytes(octets)
}

fn from_s_addr(in_addr: libc::in_addr_t) -> Ipv4Addr {
    in_addr.to_be().into()
}

fn to_in6_addr(addr: &Ipv6Addr) -> libc::in6_addr {
    let mut ret: libc::in6_addr = unsafe { mem::zeroed() };
    ret.s6_addr = addr.octets();
    return ret;
}

#[cfg(target_os = "android")]
fn to_ipv6mr_interface(value: u32) -> c_int {
    value as c_int
}

#[cfg(not(target_os = "android"))]
fn to_ipv6mr_interface(value: u32) -> libc::c_uint {
    value as libc::c_uint
}

fn linger2dur(linger_opt: libc::linger) -> Option<Duration> {
    if linger_opt.l_onoff == 0 {
        None
    } else {
        Some(Duration::from_secs(linger_opt.l_linger as u64))
    }
}

fn dur2linger(dur: Option<Duration>) -> libc::linger {
    match dur {
        Some(d) => libc::linger {
            l_onoff: 1,
            l_linger: d.as_secs() as c_int,
        },
        None => libc::linger {
            l_onoff: 0,
            l_linger: 0,
        },
    }
}

#[test]
fn test_ip() {
    let ip = Ipv4Addr::new(127, 0, 0, 1);
    assert_eq!(ip, from_s_addr(to_s_addr(&ip)));

    let ip = Ipv4Addr::new(127, 34, 4, 12);
    let want = 127 << 0 | 34 << 8 | 4 << 16 | 12 << 24;
    assert_eq!(to_s_addr(&ip), want);
    assert_eq!(from_s_addr(want), ip);
}

#[test]
#[cfg(all(feature = "all", not(target_os = "redox")))]
fn test_out_of_band_inline() {
    let tcp = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).unwrap();
    assert_eq!(tcp.out_of_band_inline().unwrap(), false);

    tcp.set_out_of_band_inline(true).unwrap();
    assert_eq!(tcp.out_of_band_inline().unwrap(), true);
}
