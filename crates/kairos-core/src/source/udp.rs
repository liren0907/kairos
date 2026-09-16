//! 帶核心接收時間戳記的 UDP socket。
//!
//! `SO_TIMESTAMP_MONOTONIC` 讓核心在封包抵達時記下 `mach_absolute_time`，
//! 透過 `recvmsg` 的 cmsg 交回來；spike 已驗證它與使用者空間讀到的值同基準同單位，
//! 且比 `recvmsg` 返回後才讀早 10 到 80 微秒。這段就是我們要避開的排程抖動。
//!
//! 送出端 macOS 沒有對應的核心時間戳記，t₁ 只能在 `sendto` 前於使用者空間讀。

use std::io;
use std::mem;
use std::net::{SocketAddrV4, ToSocketAddrs};
use std::ptr;
use std::time::Duration;

use crate::time::HostTime;

/// SDK 的 sys/socket.h 有定義（`SCM_TIMESTAMP_MONOTONIC 0x04`，資料為 uint64_t），
/// 但 libc crate 尚未收錄。
const SCM_TIMESTAMP_MONOTONIC: libc::c_int = 0x04;

/// 一次接收的結果。
#[derive(Clone, Copy, Debug)]
pub struct Received {
    pub len: usize,
    /// 核心記下的抵達時刻；cmsg 沒來時為 `None`。
    pub kernel_time: Option<HostTime>,
    /// `recvmsg` 返回後立刻讀的時刻，永遠晚於或等於真正的抵達時刻。
    pub user_time: HostTime,
}

impl Received {
    /// 當 t₄ 用的時刻：有核心時間戳記就用它，否則退回使用者空間的值。
    /// 退回的值偏晚，只會讓區間變寬，不會讓真值掉出去。
    pub fn arrival(&self) -> HostTime {
        self.kernel_time.unwrap_or(self.user_time)
    }
}

pub struct TimestampedUdpSocket {
    fd: libc::c_int,
}

impl TimestampedUdpSocket {
    /// 建立 IPv4 UDP socket，打開核心時間戳記，設定接收逾時。
    pub fn new_ipv4(read_timeout: Duration) -> io::Result<TimestampedUdpSocket> {
        // SAFETY: 標準 socket 建立與選項設定，參數皆為合法值；失敗路徑都會關閉 fd。
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let sock = TimestampedUdpSocket { fd };

            let on: libc::c_int = 1;
            sock.setsockopt(libc::SO_TIMESTAMP_MONOTONIC, &on)
                .map_err(|e| io::Error::new(e.kind(), format!("SO_TIMESTAMP_MONOTONIC：{e}")))?;

            let tv = libc::timeval {
                tv_sec: read_timeout.as_secs() as libc::time_t,
                tv_usec: read_timeout.subsec_micros() as libc::suseconds_t,
            };
            sock.setsockopt(libc::SO_RCVTIMEO, &tv)?;

            Ok(sock)
        }
    }

    unsafe fn setsockopt<T>(&self, opt: libc::c_int, val: &T) -> io::Result<()> {
        // SAFETY: 呼叫端保證 `val` 是該選項期望的型別。
        let rc = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                opt,
                val as *const T as *const libc::c_void,
                mem::size_of::<T>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 送出封包，回傳 `sendto` 前一刻讀到的本機時間 t₁。
    pub fn send_to(&self, payload: &[u8], addr: SocketAddrV4) -> io::Result<HostTime> {
        let sin = sockaddr_in_from(addr);
        let sent = HostTime::now();
        // SAFETY: 位址結構已填妥，長度一致。
        let n = unsafe {
            libc::sendto(
                self.fd,
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
                0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sent)
    }

    /// 收一個封包，同時取得核心與使用者空間的抵達時刻。
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<Received> {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        // 一個 u64 的 cmsg 只需要 CMSG_SPACE(8)，64 位元組綽綽有餘。
        let mut control = [0u8; 64];

        // SAFETY: msghdr 以零初始化後只填合法指標與長度；recvmsg 返回後
        // 只在 msg_controllen 範圍內走訪 cmsg。
        unsafe {
            let mut msg: libc::msghdr = mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = control.len() as libc::socklen_t;

            let n = libc::recvmsg(self.fd, &mut msg, 0);
            let user_time = HostTime::now();
            if n < 0 {
                return Err(io::Error::last_os_error());
            }

            let mut kernel_time = None;
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                let hdr = &*cmsg;
                if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == SCM_TIMESTAMP_MONOTONIC {
                    let ticks = ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const u64);
                    kernel_time = Some(HostTime::from_ticks(ticks));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }

            Ok(Received {
                len: n as usize,
                kernel_time,
                user_time,
            })
        }
    }
}

impl Drop for TimestampedUdpSocket {
    fn drop(&mut self) {
        // SAFETY: fd 由本型別建立，只關閉一次。
        unsafe { libc::close(self.fd) };
    }
}

fn sockaddr_in_from(addr: SocketAddrV4) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in 全零是合法的初始狀態，之後填入必要欄位。
    let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
    sin.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = addr.port().to_be();
    sin.sin_addr = libc::in_addr {
        s_addr: u32::from(*addr.ip()).to_be(),
    };
    sin
}

/// 解析主機名稱，只取 IPv4 位址。
pub fn resolve_ipv4(host: &str, port: u16) -> io::Result<Vec<SocketAddrV4>> {
    let addrs: Vec<SocketAddrV4> = (host, port)
        .to_socket_addrs()?
        .filter_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(v4),
            std::net::SocketAddr::V6(_) => None,
        })
        .collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host} 沒有 IPv4 位址"),
        ));
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn loopback_roundtrip_carries_kernel_timestamp() {
        let sock = TimestampedUdpSocket::new_ipv4(Duration::from_secs(2)).unwrap();
        // 用 std 開一個對端，把封包送回來。
        let peer = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let peer_addr = match peer.local_addr().unwrap() {
            std::net::SocketAddr::V4(v4) => v4,
            _ => unreachable!(),
        };
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // 先讓我們的 socket 送出一次，讓對端知道我們的位址。
        let sent = sock.send_to(b"ping", peer_addr).unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        peer.send_to(b"pong", from).unwrap();

        let r = sock.recv(&mut buf).unwrap();
        assert_eq!(&buf[..r.len], b"pong");
        let k = r.kernel_time.expect("loopback 應有核心時間戳記");
        assert!(k >= sent, "核心抵達時刻不應早於送出");
        assert!(k <= r.user_time, "核心抵達時刻不應晚於使用者空間讀到的值");
        assert!(r.user_time.saturating_duration_since(k) < Duration::from_millis(50));
        assert_eq!(r.arrival(), k);
    }

    #[test]
    fn recv_times_out() {
        let sock = TimestampedUdpSocket::new_ipv4(Duration::from_millis(100)).unwrap();
        let mut buf = [0u8; 16];
        let err = sock.recv(&mut buf).unwrap_err();
        assert!(matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
    }
}
