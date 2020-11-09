// Copyright 2015 The Rust Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cmp;
use std::fmt;
use std::io;
use std::io::{IoSlice, IoSliceMut, Read, Write};
use std::mem::{self, size_of_val, MaybeUninit};
use std::net::Shutdown;
use std::net::{self, Ipv4Addr, Ipv6Addr};
use std::os::windows::prelude::*;
use std::ptr;
use std::sync::Once;
use std::time::Duration;

use winapi::ctypes::{c_char, c_ulong};
use winapi::shared::in6addr::*;
use winapi::shared::inaddr::*;
use winapi::shared::minwindef::DWORD;
#[cfg(feature = "all")]
use winapi::shared::ntdef::HANDLE;
use winapi::shared::ws2def::{self, *};
use winapi::shared::ws2ipdef::*;
#[cfg(feature = "all")]
use winapi::um::handleapi::SetHandleInformation;
use winapi::um::processthreadsapi::GetCurrentProcessId;
#[cfg(feature = "all")]
use winapi::um::winbase;
use winapi::um::winbase::INFINITE;
use winapi::um::winsock2 as sock;

use crate::{RecvFlags, SockAddr};

const MSG_PEEK: c_int = 0x2;
const SD_BOTH: c_int = 2;
const SD_RECEIVE: c_int = 0;
const SD_SEND: c_int = 1;
const SIO_KEEPALIVE_VALS: DWORD = 0x98000004;

pub use winapi::ctypes::c_int;

/// Fake MSG_TRUNC flag for the [`RecvFlags`] struct.
///
/// The flag is enabled when a `WSARecv[From]` call returns `WSAEMSGSIZE`.
/// The value of the flag is defined by us.
pub(crate) const MSG_TRUNC: c_int = 0x01;

// Used in `Domain`.
pub(crate) use winapi::shared::ws2def::{AF_INET, AF_INET6};
// Used in `Type`.
pub(crate) use winapi::shared::ws2def::{SOCK_DGRAM, SOCK_STREAM};
#[cfg(feature = "all")]
pub(crate) use winapi::shared::ws2def::{SOCK_RAW, SOCK_SEQPACKET};
// Used in `Protocol`.
pub(crate) const IPPROTO_ICMP: c_int = winapi::shared::ws2def::IPPROTO_ICMP as c_int;
pub(crate) const IPPROTO_ICMPV6: c_int = winapi::shared::ws2def::IPPROTO_ICMPV6 as c_int;
pub(crate) const IPPROTO_TCP: c_int = winapi::shared::ws2def::IPPROTO_TCP as c_int;
pub(crate) const IPPROTO_UDP: c_int = winapi::shared::ws2def::IPPROTO_UDP as c_int;
// Used in `SockAddr`.
pub(crate) use winapi::shared::ws2def::{
    ADDRESS_FAMILY as sa_family_t, SOCKADDR as sockaddr, SOCKADDR_IN as sockaddr_in,
    SOCKADDR_STORAGE as sockaddr_storage,
};
pub(crate) use winapi::shared::ws2ipdef::SOCKADDR_IN6_LH as sockaddr_in6;
pub(crate) use winapi::um::ws2tcpip::socklen_t;

/// Helper macro to execute a system call that returns an `io::Result`.
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ), $err_test: path, $err_value: expr) => {{
        let res = unsafe { sock::$fn($($arg, )*) };
        if $err_test(&res, &$err_value) {
            Err(io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

impl_debug!(
    crate::Domain,
    ws2def::AF_INET,
    ws2def::AF_INET6,
    ws2def::AF_UNIX,
    ws2def::AF_UNSPEC, // = 0.
);

impl_debug!(
    crate::Type,
    ws2def::SOCK_STREAM,
    ws2def::SOCK_DGRAM,
    ws2def::SOCK_RAW,
    ws2def::SOCK_RDM,
    ws2def::SOCK_SEQPACKET,
);

impl_debug!(
    crate::Protocol,
    self::IPPROTO_ICMP,
    self::IPPROTO_ICMPV6,
    self::IPPROTO_TCP,
    self::IPPROTO_UDP,
);

impl std::fmt::Debug for RecvFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecvFlags")
            .field("is_truncated", &self.is_truncated())
            .finish()
    }
}

#[repr(C)]
struct tcp_keepalive {
    onoff: c_ulong,
    keepalivetime: c_ulong,
    keepaliveinterval: c_ulong,
}

fn init() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        // Initialize winsock through the standard library by just creating a
        // dummy socket. Whether this is successful or not we drop the result as
        // libstd will be sure to have initialized winsock.
        let _ = net::UdpSocket::bind("127.0.0.1:34254");
    });
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { sock::WSAGetLastError() })
}

// TODO: rename to `Socket` once the struct `Socket` is no longer used.
pub(crate) type SysSocket = sock::SOCKET;

pub(crate) fn socket(family: c_int, ty: c_int, protocol: c_int) -> io::Result<SysSocket> {
    init();

    syscall!(
        WSASocketW(
            family,
            ty,
            protocol,
            ptr::null_mut(),
            0,
            sock::WSA_FLAG_OVERLAPPED,
        ),
        PartialEq::eq,
        sock::INVALID_SOCKET
    )
}

pub(crate) fn bind(socket: SysSocket, addr: &SockAddr) -> io::Result<()> {
    syscall!(bind(socket, addr.as_ptr(), addr.len()), PartialEq::ne, 0).map(|_| ())
}

pub(crate) fn connect(socket: SysSocket, addr: &SockAddr) -> io::Result<()> {
    syscall!(connect(socket, addr.as_ptr(), addr.len()), PartialEq::ne, 0).map(|_| ())
}

pub(crate) fn listen(socket: SysSocket, backlog: i32) -> io::Result<()> {
    syscall!(listen(socket, backlog), PartialEq::ne, 0).map(|_| ())
}

pub(crate) fn accept(socket: SysSocket) -> io::Result<(SysSocket, SockAddr)> {
    // Safety: zeroed `SOCKADDR_STORAGE` is valid.
    let mut storage: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
    let mut len = size_of_val(&storage) as c_int;
    syscall!(
        accept(socket, &mut storage as *mut _ as *mut _, &mut len),
        PartialEq::eq,
        sock::INVALID_SOCKET
    )
    .map(|socket| {
        let addr = unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, len) };
        (socket, addr)
    })
}

pub(crate) fn getsockname(socket: SysSocket) -> io::Result<SockAddr> {
    // Safety: zeroed `SOCKADDR_STORAGE` is valid.
    let mut storage: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
    let mut len = size_of_val(&storage) as c_int;
    syscall!(
        getsockname(socket, &mut storage as *mut _ as *mut _, &mut len),
        PartialEq::eq,
        sock::SOCKET_ERROR
    )
    .map(|_| unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, len) })
}

pub(crate) fn getpeername(socket: SysSocket) -> io::Result<SockAddr> {
    // Safety: zeroed `SOCKADDR_STORAGE` is valid.
    let mut storage: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
    let mut len = size_of_val(&storage) as c_int;
    syscall!(
        getpeername(socket, &mut storage as *mut _ as *mut _, &mut len),
        PartialEq::eq,
        sock::SOCKET_ERROR
    )
    .map(|_| unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, len) })
}

pub(crate) fn try_clone(socket: SysSocket) -> io::Result<SysSocket> {
    let mut info: MaybeUninit<sock::WSAPROTOCOL_INFOW> = MaybeUninit::uninit();
    syscall!(
        WSADuplicateSocketW(socket, GetCurrentProcessId(), info.as_mut_ptr()),
        PartialEq::eq,
        sock::SOCKET_ERROR
    )?;
    // Safety: `WSADuplicateSocketW` intialised `info` for us.
    let mut info = unsafe { info.assume_init() };

    syscall!(
        WSASocketW(
            info.iAddressFamily,
            info.iSocketType,
            info.iProtocol,
            &mut info,
            0,
            sock::WSA_FLAG_OVERLAPPED | sock::WSA_FLAG_NO_HANDLE_INHERIT,
        ),
        PartialEq::eq,
        sock::INVALID_SOCKET
    )
}

/// Windows only API.
impl crate::Socket {
    /// Sets `HANDLE_FLAG_INHERIT` to zero using `SetHandleInformation`.
    #[cfg(feature = "all")]
    pub fn set_no_inherit(&self) -> io::Result<()> {
        // NOTE: can't use `syscall!` because it expects the function in the
        // `sock::` path.
        let res =
            unsafe { SetHandleInformation(self.inner as HANDLE, winbase::HANDLE_FLAG_INHERIT, 0) };
        if res == 0 {
            // Zero means error.
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[repr(transparent)] // Required during rewriting.
pub struct Socket {
    socket: SysSocket,
}

impl Socket {
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_ERROR)?;
            if raw == 0 {
                Ok(None)
            } else {
                Ok(Some(io::Error::from_raw_os_error(raw as i32)))
            }
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        unsafe {
            let mut nonblocking = nonblocking as c_ulong;
            let r = sock::ioctlsocket(self.socket, sock::FIONBIO as c_int, &mut nonblocking);
            if r == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Write => SD_SEND,
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Both => SD_BOTH,
        };
        if unsafe { sock::shutdown(self.socket, how) == 0 } {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    pub fn recv(&self, buf: &mut [u8], flags: c_int) -> io::Result<usize> {
        unsafe {
            let n = {
                sock::recv(
                    self.socket,
                    buf.as_mut_ptr() as *mut c_char,
                    clamp(buf.len()),
                    flags,
                )
            };
            match n {
                sock::SOCKET_ERROR if sock::WSAGetLastError() == sock::WSAESHUTDOWN as i32 => Ok(0),
                sock::SOCKET_ERROR => Err(last_error()),
                n => Ok(n as usize),
            }
        }
    }

    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let n = {
                sock::recv(
                    self.socket,
                    buf.as_mut_ptr() as *mut c_char,
                    clamp(buf.len()),
                    MSG_PEEK,
                )
            };
            match n {
                sock::SOCKET_ERROR if sock::WSAGetLastError() == sock::WSAESHUTDOWN as i32 => Ok(0),
                sock::SOCKET_ERROR => Err(last_error()),
                n => Ok(n as usize),
            }
        }
    }

    pub fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SockAddr)> {
        self.recv_from(buf, MSG_PEEK)
    }

    pub fn recv_from(&self, buf: &mut [u8], flags: c_int) -> io::Result<(usize, SockAddr)> {
        unsafe {
            let mut storage: SOCKADDR_STORAGE = mem::zeroed();
            let mut addrlen = mem::size_of_val(&storage) as c_int;

            let n = {
                sock::recvfrom(
                    self.socket,
                    buf.as_mut_ptr() as *mut c_char,
                    clamp(buf.len()),
                    flags,
                    &mut storage as *mut _ as *mut _,
                    &mut addrlen,
                )
            };
            let n = match n {
                sock::SOCKET_ERROR if sock::WSAGetLastError() == sock::WSAESHUTDOWN as i32 => 0,
                sock::SOCKET_ERROR => return Err(last_error()),
                n => n as usize,
            };
            let addr = SockAddr::from_raw_parts(&storage as *const _ as *const _, addrlen);
            Ok((n, addr))
        }
    }

    pub fn recv_vectored(
        &self,
        bufs: &mut [IoSliceMut<'_>],
        flags: c_int,
    ) -> io::Result<(usize, RecvFlags)> {
        let mut nread = 0;
        let mut flags = flags as DWORD;
        let ret = unsafe {
            sock::WSARecv(
                self.socket,
                bufs.as_mut_ptr() as *mut WSABUF,
                bufs.len().min(DWORD::MAX as usize) as DWORD,
                &mut nread,
                &mut flags,
                ptr::null_mut(),
                None,
            )
        };

        let nread = nread as usize;
        if ret == 0 {
            Ok((nread, RecvFlags(0)))
        } else {
            let error = last_error();
            match error.raw_os_error() {
                Some(sock::WSAESHUTDOWN) => Ok((0, RecvFlags(0))),
                Some(sock::WSAEMSGSIZE) => Ok((nread, RecvFlags(MSG_TRUNC))),
                _ => Err(error),
            }
        }
    }

    pub fn recv_from_vectored(
        &self,
        bufs: &mut [IoSliceMut<'_>],
        flags: c_int,
    ) -> io::Result<(usize, RecvFlags, SockAddr)> {
        let mut nread = 0;
        let mut flags = flags as DWORD;
        let mut storage: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
        let mut addrlen = mem::size_of_val(&storage) as c_int;
        let ret = unsafe {
            sock::WSARecvFrom(
                self.socket,
                bufs.as_mut_ptr() as *mut WSABUF,
                bufs.len().min(DWORD::MAX as usize) as DWORD,
                &mut nread,
                &mut flags,
                &mut storage as *mut SOCKADDR_STORAGE as *mut SOCKADDR,
                &mut addrlen,
                ptr::null_mut(),
                None,
            )
        };

        let flags;
        if ret == 0 {
            flags = RecvFlags(0);
        } else {
            let error = last_error();
            if error.raw_os_error() == Some(sock::WSAEMSGSIZE) {
                flags = RecvFlags(MSG_TRUNC)
            } else {
                return Err(error);
            }
        }

        let addr = unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, addrlen) };
        Ok((nread as usize, flags, addr))
    }

    pub fn send(&self, buf: &[u8], flags: c_int) -> io::Result<usize> {
        unsafe {
            let n = {
                sock::send(
                    self.socket,
                    buf.as_ptr() as *const c_char,
                    clamp(buf.len()),
                    flags,
                )
            };
            if n == sock::SOCKET_ERROR {
                Err(last_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    pub fn send_to(&self, buf: &[u8], flags: c_int, addr: &SockAddr) -> io::Result<usize> {
        unsafe {
            let n = {
                sock::sendto(
                    self.socket,
                    buf.as_ptr() as *const c_char,
                    clamp(buf.len()),
                    flags,
                    addr.as_ptr(),
                    addr.len(),
                )
            };
            if n == sock::SOCKET_ERROR {
                Err(last_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    pub fn send_vectored(&self, bufs: &[IoSlice<'_>], flags: c_int) -> io::Result<usize> {
        let mut nsent = 0;
        let ret = unsafe {
            sock::WSASend(
                self.socket,
                bufs.as_ptr() as *mut WSABUF,
                bufs.len().min(DWORD::MAX as usize) as DWORD,
                &mut nsent,
                flags as DWORD,
                std::ptr::null_mut(),
                None,
            )
        };
        match ret {
            0 => Ok(nsent as usize),
            _ => Err(last_error()),
        }
    }

    pub fn send_to_vectored(
        &self,
        bufs: &[IoSlice<'_>],
        flags: c_int,
        addr: &SockAddr,
    ) -> io::Result<usize> {
        let mut nsent = 0;
        let ret = unsafe {
            sock::WSASendTo(
                self.socket,
                bufs.as_ptr() as *mut WSABUF,
                bufs.len().min(DWORD::MAX as usize) as DWORD,
                &mut nsent,
                flags as DWORD,
                addr.as_ptr(),
                addr.len(),
                std::ptr::null_mut(),
                None,
            )
        };
        match ret {
            0 => Ok(nsent as usize),
            _ => Err(last_error()),
        }
    }

    // ================================================

    pub fn ttl(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IP, IP_TTL)?;
            Ok(raw as u32)
        }
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IP, IP_TTL, ttl as c_int) }
    }

    pub fn unicast_hops_v6(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6 as c_int, IPV6_UNICAST_HOPS)?;
            Ok(raw as u32)
        }
    }

    pub fn set_unicast_hops_v6(&self, hops: u32) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IPV6 as c_int, IPV6_UNICAST_HOPS, hops as c_int) }
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6 as c_int, IPV6_V6ONLY)?;
            Ok(raw != 0)
        }
    }

    pub fn set_only_v6(&self, only_v6: bool) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IPV6 as c_int, IPV6_V6ONLY, only_v6 as c_int) }
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        unsafe { Ok(ms2dur(self.getsockopt(SOL_SOCKET, SO_RCVTIMEO)?)) }
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe { self.setsockopt(SOL_SOCKET, SO_RCVTIMEO, dur2ms(dur)?) }
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        unsafe { Ok(ms2dur(self.getsockopt(SOL_SOCKET, SO_SNDTIMEO)?)) }
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe { self.setsockopt(SOL_SOCKET, SO_SNDTIMEO, dur2ms(dur)?) }
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_char = self.getsockopt(IPPROTO_TCP, TCP_NODELAY)?;
            Ok(raw != 0)
        }
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_TCP, TCP_NODELAY, nodelay as c_char) }
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_BROADCAST)?;
            Ok(raw != 0)
        }
    }

    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        unsafe { self.setsockopt(SOL_SOCKET, SO_BROADCAST, broadcast as c_int) }
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IP, IP_MULTICAST_LOOP)?;
            Ok(raw != 0)
        }
    }

    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IP, IP_MULTICAST_LOOP, multicast_loop_v4 as c_int) }
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IP, IP_MULTICAST_TTL)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IP, IP_MULTICAST_TTL, multicast_ttl_v4 as c_int) }
    }

    pub fn multicast_hops_v6(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6 as c_int, IPV6_MULTICAST_HOPS)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_hops_v6(&self, hops: u32) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IPV6 as c_int, IPV6_MULTICAST_HOPS, hops as c_int) }
    }

    pub fn multicast_if_v4(&self) -> io::Result<Ipv4Addr> {
        unsafe {
            let imr_interface: IN_ADDR = self.getsockopt(IPPROTO_IP, IP_MULTICAST_IF)?;
            Ok(from_in_addr(imr_interface))
        }
    }

    pub fn set_multicast_if_v4(&self, interface: &Ipv4Addr) -> io::Result<()> {
        let imr_interface = to_in_addr(interface);

        unsafe { self.setsockopt(IPPROTO_IP, IP_MULTICAST_IF, imr_interface) }
    }

    pub fn multicast_if_v6(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6 as c_int, IPV6_MULTICAST_IF)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_if_v6(&self, interface: u32) -> io::Result<()> {
        unsafe { self.setsockopt(IPPROTO_IPV6 as c_int, IPV6_MULTICAST_IF, interface as c_int) }
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6 as c_int, IPV6_MULTICAST_LOOP)?;
            Ok(raw != 0)
        }
    }

    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(
                IPPROTO_IPV6 as c_int,
                IPV6_MULTICAST_LOOP,
                multicast_loop_v6 as c_int,
            )
        }
    }

    pub fn join_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        let mreq = IP_MREQ {
            imr_multiaddr: to_in_addr(multiaddr),
            imr_interface: to_in_addr(interface),
        };
        unsafe { self.setsockopt(IPPROTO_IP, IP_ADD_MEMBERSHIP, mreq) }
    }

    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        let multiaddr = to_in6_addr(multiaddr);
        let mreq = IPV6_MREQ {
            ipv6mr_multiaddr: multiaddr,
            ipv6mr_interface: interface,
        };
        unsafe { self.setsockopt(IPPROTO_IPV6 as c_int, IPV6_ADD_MEMBERSHIP, mreq) }
    }

    pub fn leave_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        let mreq = IP_MREQ {
            imr_multiaddr: to_in_addr(multiaddr),
            imr_interface: to_in_addr(interface),
        };
        unsafe { self.setsockopt(IPPROTO_IP, IP_DROP_MEMBERSHIP, mreq) }
    }

    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        let multiaddr = to_in6_addr(multiaddr);
        let mreq = IPV6_MREQ {
            ipv6mr_multiaddr: multiaddr,
            ipv6mr_interface: interface,
        };
        unsafe { self.setsockopt(IPPROTO_IP, IPV6_DROP_MEMBERSHIP, mreq) }
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsafe { Ok(linger2dur(self.getsockopt(SOL_SOCKET, SO_LINGER)?)) }
    }

    pub fn set_linger(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe { self.setsockopt(SOL_SOCKET, SO_LINGER, dur2linger(dur)) }
    }

    pub fn set_reuse_address(&self, reuse: bool) -> io::Result<()> {
        unsafe { self.setsockopt(SOL_SOCKET, SO_REUSEADDR, reuse as c_int) }
    }

    pub fn reuse_address(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_REUSEADDR)?;
            Ok(raw != 0)
        }
    }

    pub fn recv_buffer_size(&self) -> io::Result<usize> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_RCVBUF)?;
            Ok(raw as usize)
        }
    }

    pub fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
        unsafe {
            // TODO: casting usize to a c_int should be a checked cast
            self.setsockopt(SOL_SOCKET, SO_RCVBUF, size as c_int)
        }
    }

    pub fn send_buffer_size(&self) -> io::Result<usize> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_SNDBUF)?;
            Ok(raw as usize)
        }
    }

    pub fn set_send_buffer_size(&self, size: usize) -> io::Result<()> {
        unsafe {
            // TODO: casting usize to a c_int should be a checked cast
            self.setsockopt(SOL_SOCKET, SO_SNDBUF, size as c_int)
        }
    }

    pub fn keepalive(&self) -> io::Result<Option<Duration>> {
        let mut ka = tcp_keepalive {
            onoff: 0,
            keepalivetime: 0,
            keepaliveinterval: 0,
        };
        let n = unsafe {
            sock::WSAIoctl(
                self.socket,
                SIO_KEEPALIVE_VALS,
                0 as *mut _,
                0,
                &mut ka as *mut _ as *mut _,
                mem::size_of_val(&ka) as DWORD,
                0 as *mut _,
                0 as *mut _,
                None,
            )
        };
        if n == 0 {
            Ok(if ka.onoff == 0 {
                None
            } else if ka.keepaliveinterval == 0 {
                None
            } else {
                let seconds = ka.keepaliveinterval / 1000;
                let nanos = (ka.keepaliveinterval % 1000) * 1_000_000;
                Some(Duration::new(seconds as u64, nanos as u32))
            })
        } else {
            Err(last_error())
        }
    }

    pub fn set_keepalive(&self, keepalive: Option<Duration>) -> io::Result<()> {
        let ms = dur2ms(keepalive)?;
        // TODO: checked casts here
        let ka = tcp_keepalive {
            onoff: keepalive.is_some() as c_ulong,
            keepalivetime: ms as c_ulong,
            keepaliveinterval: ms as c_ulong,
        };
        let mut out = 0;
        let n = unsafe {
            sock::WSAIoctl(
                self.socket,
                SIO_KEEPALIVE_VALS,
                &ka as *const _ as *mut _,
                mem::size_of_val(&ka) as DWORD,
                0 as *mut _,
                0,
                &mut out,
                0 as *mut _,
                None,
            )
        };
        if n == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    pub fn out_of_band_inline(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_OOBINLINE)?;
            Ok(raw != 0)
        }
    }

    pub fn set_out_of_band_inline(&self, oob_inline: bool) -> io::Result<()> {
        unsafe { self.setsockopt(SOL_SOCKET, SO_OOBINLINE, oob_inline as c_int) }
    }

    unsafe fn setsockopt<T>(&self, opt: c_int, val: c_int, payload: T) -> io::Result<()>
    where
        T: Copy,
    {
        let payload = &payload as *const T as *const c_char;
        if sock::setsockopt(self.socket, opt, val, payload, mem::size_of::<T>() as c_int) == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    unsafe fn getsockopt<T: Copy>(&self, opt: c_int, val: c_int) -> io::Result<T> {
        let mut slot: T = mem::zeroed();
        let mut len = mem::size_of::<T>() as c_int;
        if sock::getsockopt(
            self.socket,
            opt,
            val,
            &mut slot as *mut _ as *mut _,
            &mut len,
        ) == 0
        {
            assert_eq!(len as usize, mem::size_of::<T>());
            Ok(slot)
        } else {
            Err(last_error())
        }
    }

    pub fn inner(self) -> SysSocket {
        self.socket
    }

    pub fn from_inner(socket: SysSocket) -> Socket {
        Socket { socket }
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        <&Socket>::read(&mut &*self, buf)
    }
}

impl<'a> Read for &'a Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf, 0)
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
        f.field("socket", &self.socket);
        if let Ok(addr) = getsockname(self.socket) {
            f.field("local_addr", &addr);
        }
        if let Ok(addr) = getpeername(self.socket) {
            f.field("peer_addr", &addr);
        }
        f.finish()
    }
}

impl AsRawSocket for Socket {
    fn as_raw_socket(&self) -> RawSocket {
        self.socket as RawSocket
    }
}

impl IntoRawSocket for Socket {
    fn into_raw_socket(self) -> RawSocket {
        let socket = self.socket;
        mem::forget(self);
        socket as RawSocket
    }
}

impl FromRawSocket for Socket {
    unsafe fn from_raw_socket(socket: RawSocket) -> Socket {
        Socket {
            socket: socket as sock::SOCKET,
        }
    }
}

impl AsRawSocket for crate::Socket {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner as RawSocket
    }
}

impl IntoRawSocket for crate::Socket {
    fn into_raw_socket(self) -> RawSocket {
        let socket = self.inner;
        mem::forget(self);
        socket as RawSocket
    }
}

impl FromRawSocket for crate::Socket {
    unsafe fn from_raw_socket(socket: RawSocket) -> crate::Socket {
        crate::Socket {
            inner: Socket::from_raw_socket(socket).inner(),
        }
    }
}

impl From<Socket> for net::TcpStream {
    fn from(socket: Socket) -> net::TcpStream {
        unsafe { net::TcpStream::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<Socket> for net::TcpListener {
    fn from(socket: Socket) -> net::TcpListener {
        unsafe { net::TcpListener::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<Socket> for net::UdpSocket {
    fn from(socket: Socket) -> net::UdpSocket {
        unsafe { net::UdpSocket::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<net::TcpStream> for Socket {
    fn from(socket: net::TcpStream) -> Socket {
        unsafe { Socket::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<net::TcpListener> for Socket {
    fn from(socket: net::TcpListener) -> Socket {
        unsafe { Socket::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<net::UdpSocket> for Socket {
    fn from(socket: net::UdpSocket) -> Socket {
        unsafe { Socket::from_raw_socket(socket.into_raw_socket()) }
    }
}

pub(crate) fn close(socket: SysSocket) {
    unsafe {
        let _ = sock::closesocket(socket);
    }
}

fn clamp(input: usize) -> c_int {
    cmp::min(input, <c_int>::max_value() as usize) as c_int
}

fn dur2ms(dur: Option<Duration>) -> io::Result<DWORD> {
    match dur {
        Some(dur) => {
            // Note that a duration is a (u64, u32) (seconds, nanoseconds)
            // pair, and the timeouts in windows APIs are typically u32
            // milliseconds. To translate, we have two pieces to take care of:
            //
            // * Nanosecond precision is rounded up
            // * Greater than u32::MAX milliseconds (50 days) is rounded up to
            //   INFINITE (never time out).
            let ms = dur
                .as_secs()
                .checked_mul(1000)
                .and_then(|ms| ms.checked_add((dur.subsec_nanos() as u64) / 1_000_000))
                .and_then(|ms| {
                    ms.checked_add(if dur.subsec_nanos() % 1_000_000 > 0 {
                        1
                    } else {
                        0
                    })
                })
                .map(|ms| {
                    if ms > <DWORD>::max_value() as u64 {
                        INFINITE
                    } else {
                        ms as DWORD
                    }
                })
                .unwrap_or(INFINITE);
            if ms == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot set a 0 duration timeout",
                ));
            }
            Ok(ms)
        }
        None => Ok(0),
    }
}

fn ms2dur(raw: DWORD) -> Option<Duration> {
    if raw == 0 {
        None
    } else {
        let secs = raw / 1000;
        let nsec = (raw % 1000) * 1000000;
        Some(Duration::new(secs as u64, nsec as u32))
    }
}

pub(crate) fn to_in_addr(addr: &Ipv4Addr) -> IN_ADDR {
    let mut s_un: in_addr_S_un = unsafe { mem::zeroed() };
    // `S_un` is stored as BE on all machines, and the array is in BE order.
    // So the native endian conversion method is used so that it's never swapped.
    unsafe { *(s_un.S_addr_mut()) = u32::from_ne_bytes(addr.octets()) };
    IN_ADDR { S_un: s_un }
}

pub(crate) fn from_in_addr(in_addr: IN_ADDR) -> Ipv4Addr {
    Ipv4Addr::from(unsafe { *in_addr.S_un.S_addr() }.to_ne_bytes())
}

pub(crate) fn to_in6_addr(addr: &Ipv6Addr) -> in6_addr {
    let mut ret_addr: in6_addr_u = unsafe { mem::zeroed() };
    unsafe { *(ret_addr.Byte_mut()) = addr.octets() };
    let mut ret: in6_addr = unsafe { mem::zeroed() };
    ret.u = ret_addr;
    ret
}

pub(crate) fn from_in6_addr(in6_addr: in6_addr) -> Ipv6Addr {
    Ipv6Addr::from(*unsafe { in6_addr.u.Byte() })
}

fn linger2dur(linger_opt: sock::linger) -> Option<Duration> {
    if linger_opt.l_onoff == 0 {
        None
    } else {
        Some(Duration::from_secs(linger_opt.l_linger as u64))
    }
}

fn dur2linger(dur: Option<Duration>) -> sock::linger {
    match dur {
        Some(d) => sock::linger {
            l_onoff: 1,
            l_linger: d.as_secs() as u16,
        },
        None => sock::linger {
            l_onoff: 0,
            l_linger: 0,
        },
    }
}

#[test]
fn test_ipv4() {
    let ip = Ipv4Addr::new(127, 0, 0, 1);
    assert_eq!(ip, from_in_addr(to_in_addr(&ip)));

    let ip = Ipv4Addr::new(127, 34, 4, 12);
    let want = 127 << 0 | 34 << 8 | 4 << 16 | 12 << 24;
    assert_eq!(unsafe { *to_in_addr(&ip).S_un.S_addr() }, want);
    let mut addr: in_addr_S_un = unsafe { mem::zeroed() };
    unsafe { *(addr.S_addr_mut()) = want };
    assert_eq!(from_in_addr(IN_ADDR { S_un: addr }), ip);
}

#[test]
fn test_ipv6() {
    let ip = Ipv6Addr::new(0x2000, 1, 2, 3, 4, 5, 6, 7);
    assert_eq!(ip, from_in6_addr(to_in6_addr(&ip)));

    let ip = Ipv6Addr::new(0x2000, 1, 2, 3, 4, 5, 6, 7);
    let want = [
        0x2000u16.to_be(),
        1u16.to_be(),
        2u16.to_be(),
        3u16.to_be(),
        4u16.to_be(),
        5u16.to_be(),
        6u16.to_be(),
        7u16.to_be(),
    ];
    assert_eq!(unsafe { *to_in6_addr(&ip).u.Word() }, want);
    let mut addr: in6_addr_u = unsafe { mem::zeroed() };
    unsafe { *(addr.Word_mut()) = want };
    assert_eq!(from_in6_addr(IN6_ADDR { u: addr }), ip);
}

#[test]
fn test_out_of_band_inline() {
    let tcp = Socket {
        socket: socket(AF_INET, SOCK_STREAM, 0).unwrap(),
    };
    assert_eq!(tcp.out_of_band_inline().unwrap(), false);

    tcp.set_out_of_band_inline(true).unwrap();
    assert_eq!(tcp.out_of_band_inline().unwrap(), true);
}
