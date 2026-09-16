//! NTP 交換：把 [`udp`](super::udp) 的 I/O 與 [`ntp`](super::ntp) 的純函數接起來。

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::model::SourceKind;
use crate::source::ntp::{Exchange, NtpError, NtpRequest, NtpResponse, PACKET_LEN, sample_from_exchange};
use crate::source::udp::TimestampedUdpSocket;

#[derive(Debug)]
pub enum QueryError {
    Io(io::Error),
    Ntp(NtpError),
    /// 逾時前沒有收到 origin 吻合的回應。
    NoMatchingResponse,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Io(e) => write!(f, "I/O：{e}"),
            QueryError::Ntp(e) => write!(f, "NTP：{e}"),
            QueryError::NoMatchingResponse => write!(f, "逾時前沒有吻合的回應"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<io::Error> for QueryError {
    fn from(e: io::Error) -> Self {
        QueryError::Io(e)
    }
}

impl From<NtpError> for QueryError {
    fn from(e: NtpError) -> Self {
        QueryError::Ntp(e)
    }
}

/// 每次呼叫給一個不重複、不可預測的 nonce。
///
/// `RandomState` 的雜湊種子每個程序不同，再混一個遞增計數器保證程序內不重複。
/// 這不是密碼學用途，只要伺服器抄回來時能對得上、外人猜不到即可。
fn next_nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(n);
    let nonce = h.finish();
    // 零在 NTP 裡代表「沒有時間戳記」，避開它。
    if nonce == 0 { 1 } else { nonce }
}

/// 對一台伺服器做一次交換。
///
/// 收到 origin 不吻合的封包（遲到的舊回應、雜訊）會略過繼續等，直到 socket 逾時。
pub fn query(
    sock: &TimestampedUdpSocket,
    addr: SocketAddrV4,
    source: SourceKind,
) -> Result<Exchange, QueryError> {
    let nonce = next_nonce();
    let sent = sock.send_to(&NtpRequest { nonce }.encode(), addr)?;

    let mut buf = [0u8; 128];
    loop {
        let r = match sock.recv(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(QueryError::NoMatchingResponse);
            }
            Err(e) => return Err(e.into()),
        };
        if r.len < PACKET_LEN {
            continue;
        }
        let response = NtpResponse::parse(&buf[..r.len])?;
        if response.origin.0 != nonce {
            continue;
        }
        return Ok(sample_from_exchange(source, nonce, sent, r.arrival(), response)?);
    }
}

/// 對同一台伺服器連續做幾次交換，每次間隔 `spacing`。
/// 回傳每次的結果，成功失敗都保留，讓呼叫端決定怎麼記錄。
pub fn query_burst(
    sock: &TimestampedUdpSocket,
    addr: SocketAddrV4,
    source: SourceKind,
    count: usize,
    spacing: Duration,
) -> Vec<Result<Exchange, QueryError>> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(query(sock, addr, source));
        if i + 1 < count {
            std::thread::sleep(spacing);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_unique_and_nonzero() {
        let a = next_nonce();
        let b = next_nonce();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b);
    }
}
