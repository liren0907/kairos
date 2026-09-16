//! NTP v4 客戶端封包（RFC 5905）：編碼請求、解析回應、把四個時間戳記變成 [`Sample`]。
//!
//! 這裡沒有任何 I/O，全部是純函數，可以直接用向量測試。
//!
//! 送出的封包在 transmit 欄位放的是隨機 nonce 而不是本機時間：伺服器會原樣抄回
//! origin 欄位，我們用它比對回應、也不把本機時鐘洩漏出去。

use std::fmt;
use std::time::Duration;

use crate::model::SourceKind;
use crate::source::Sample;
use crate::time::HostTime;

pub const PACKET_LEN: usize = 48;

/// 1900-01-01 到 1970-01-01 的秒數。
const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// 32 位元小數換奈秒：ns = frac × 10⁹ ÷ 2³²。
const FRAC_TO_NS: u128 = 1_000_000_000;

/// 伺服器回應裡的 stratum 上限；超過視為不可用。
const MAX_STRATUM: u8 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NtpError {
    /// 回應長度不足 48 位元組。
    TooShort(usize),
    /// 版本不是 3 或 4。
    BadVersion(u8),
    /// mode 不是 4（server）。
    BadMode(u8),
    /// 伺服器自報未同步（LI = 3）。
    Unsynchronized,
    /// Kiss-of-Death：stratum 0，附四字元代碼（如 RATE、DENY）。
    KissOfDeath(String),
    /// stratum 超過 15。
    BadStratum(u8),
    /// origin 欄位與送出的 nonce 不符，不是給這筆請求的回應。
    OriginMismatch,
    /// 接收或送出時間戳記為零。
    ZeroTimestamp,
    /// 伺服器的送出早於接收，或本機的收到早於送出。
    NonMonotonic,
    /// 區間寬度為負：伺服器處理時間長於往返延遲，不可能。
    InvertedInterval,
}

impl fmt::Display for NtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NtpError::TooShort(n) => write!(f, "回應只有 {n} 位元組"),
            NtpError::BadVersion(v) => write!(f, "NTP 版本 {v} 不支援"),
            NtpError::BadMode(m) => write!(f, "mode {m} 不是伺服器回應"),
            NtpError::Unsynchronized => write!(f, "伺服器未同步（LI=3）"),
            NtpError::KissOfDeath(code) => write!(f, "伺服器拒絕：{code}"),
            NtpError::BadStratum(s) => write!(f, "stratum {s} 超出範圍"),
            NtpError::OriginMismatch => write!(f, "origin 與 nonce 不符"),
            NtpError::ZeroTimestamp => write!(f, "時間戳記為零"),
            NtpError::NonMonotonic => write!(f, "時間戳記順序矛盾"),
            NtpError::InvertedInterval => write!(f, "區間寬度為負"),
        }
    }
}

impl std::error::Error for NtpError {}

/// NTP 64 位元時間戳記：高 32 位是 1900 紀元起的秒數，低 32 位是秒的小數。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NtpTimestamp(pub u64);

impl NtpTimestamp {
    pub const ZERO: NtpTimestamp = NtpTimestamp(0);

    pub fn from_be_bytes(b: [u8; 8]) -> NtpTimestamp {
        NtpTimestamp(u64::from_be_bytes(b))
    }

    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// 換成 Unix 紀元起的奈秒。
    ///
    /// era 處理：秒數小於 1970 的偏移量時視為 era 1（2036 年之後），
    /// 這樣到 2106 年都正確；沒有伺服器會送出 1900 到 1970 之間的時間。
    pub fn to_unix_ns(self) -> i128 {
        let secs = self.0 >> 32;
        let frac = self.0 & 0xFFFF_FFFF;
        let secs = if secs < NTP_UNIX_OFFSET_SECS {
            secs + (1u64 << 32)
        } else {
            secs
        };
        let unix_secs = (secs - NTP_UNIX_OFFSET_SECS) as i128;
        let frac_ns = ((frac as u128 * FRAC_TO_NS) >> 32) as i128;
        unix_secs * 1_000_000_000 + frac_ns
    }

    /// 從 Unix 奈秒建立（測試與探針用）。
    pub fn from_unix_ns(unix_ns: i128) -> NtpTimestamp {
        assert!(unix_ns >= 0, "不支援 1970 之前的時間");
        let secs = (unix_ns / 1_000_000_000) as u64 + NTP_UNIX_OFFSET_SECS;
        let ns = (unix_ns % 1_000_000_000) as u128;
        let frac = ((ns << 32) / FRAC_TO_NS) as u64;
        NtpTimestamp(((secs & 0xFFFF_FFFF) << 32) | frac)
    }
}

/// NTP 32 位元短格式（16.16 定點秒）換奈秒，用於 root delay 與 root dispersion。
fn short_to_ns(v: u32) -> u64 {
    ((v as u128 * 1_000_000_000) >> 16) as u64
}

/// 客戶端請求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NtpRequest {
    /// 放在 transmit 欄位的隨機值，伺服器會抄回 origin。
    pub nonce: u64,
}

impl NtpRequest {
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        // LI = 0、VN = 4、Mode = 3（client）
        b[0] = 0b00_100_011;
        b[40..48].copy_from_slice(&self.nonce.to_be_bytes());
        b
    }
}

/// 解析後的伺服器回應。時間戳記已換成 Unix 奈秒。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NtpResponse {
    pub leap_indicator: u8,
    pub version: u8,
    pub stratum: u8,
    /// 2 的冪次，伺服器時鐘精度。
    pub precision: i8,
    pub root_delay_ns: u64,
    pub root_dispersion_ns: u64,
    pub reference_id: [u8; 4],
    /// 原樣的 origin 欄位，應等於請求的 nonce。
    pub origin: NtpTimestamp,
    /// t₂：伺服器收到請求的時刻（Unix 奈秒）。
    pub receive_unix_ns: i128,
    /// t₃：伺服器送出回應的時刻（Unix 奈秒）。
    pub transmit_unix_ns: i128,
}

impl NtpResponse {
    pub fn parse(b: &[u8]) -> Result<NtpResponse, NtpError> {
        if b.len() < PACKET_LEN {
            return Err(NtpError::TooShort(b.len()));
        }
        let leap_indicator = b[0] >> 6;
        let version = (b[0] >> 3) & 0b111;
        let mode = b[0] & 0b111;
        let stratum = b[1];

        if !(3..=4).contains(&version) {
            return Err(NtpError::BadVersion(version));
        }
        if mode != 4 {
            return Err(NtpError::BadMode(mode));
        }
        let reference_id = [b[12], b[13], b[14], b[15]];
        if stratum == 0 {
            let code = String::from_utf8_lossy(&reference_id).into_owned();
            return Err(NtpError::KissOfDeath(code));
        }
        if stratum > MAX_STRATUM {
            return Err(NtpError::BadStratum(stratum));
        }
        if leap_indicator == 3 {
            return Err(NtpError::Unsynchronized);
        }

        let ts = |off: usize| NtpTimestamp::from_be_bytes(b[off..off + 8].try_into().unwrap());
        let origin = ts(24);
        let receive = ts(32);
        let transmit = ts(40);
        if receive.is_zero() || transmit.is_zero() {
            return Err(NtpError::ZeroTimestamp);
        }

        Ok(NtpResponse {
            leap_indicator,
            version,
            stratum,
            precision: b[3] as i8,
            root_delay_ns: short_to_ns(u32::from_be_bytes([b[4], b[5], b[6], b[7]])),
            root_dispersion_ns: short_to_ns(u32::from_be_bytes([b[8], b[9], b[10], b[11]])),
            reference_id,
            origin,
            receive_unix_ns: receive.to_unix_ns(),
            transmit_unix_ns: transmit.to_unix_ns(),
        })
    }

    /// 伺服器自報的最大誤差：root dispersion 加 root delay 的一半。
    pub fn server_error_ns(&self) -> u64 {
        self.root_dispersion_ns + self.root_delay_ns / 2
    }

    /// reference id 的可讀形式：stratum 1 是四字元代碼，其他是上游 IPv4。
    pub fn reference_id_string(&self) -> String {
        if self.stratum == 1 {
            String::from_utf8_lossy(&self.reference_id)
                .trim_end_matches('\0')
                .to_string()
        } else {
            let r = self.reference_id;
            format!("{}.{}.{}.{}", r[0], r[1], r[2], r[3])
        }
    }
}

/// 一次完整交換的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    pub sample: Sample,
    pub response: NtpResponse,
    /// t₁：本機送出時刻。
    pub sent: HostTime,
    /// t₄：本機收到時刻。
    pub received: HostTime,
    /// 往返延遲 t₄ − t₁。
    pub round_trip: Duration,
}

/// 把四個時間戳記變成一筆樣本。
///
/// 推導：單程延遲非負，所以 θ ∈ [t₃ − t₄, t₂ − t₁]；再往外加伺服器自報的誤差。
/// `sent` 與 `received` 是本機單調時間，換成「開機起的奈秒」與遠端的 Unix 奈秒相減，
/// 得到的 θ 因此是「Unix 奈秒 − 開機起奈秒」，在整個程式裡定義一致。
pub fn sample_from_exchange(
    source: SourceKind,
    nonce: u64,
    sent: HostTime,
    received: HostTime,
    response: NtpResponse,
) -> Result<Exchange, NtpError> {
    if response.origin.0 != nonce {
        return Err(NtpError::OriginMismatch);
    }
    if received < sent || response.transmit_unix_ns < response.receive_unix_ns {
        return Err(NtpError::NonMonotonic);
    }

    let t1 = sent.as_nanos() as i128;
    let t4 = received.as_nanos() as i128;
    let t2 = response.receive_unix_ns;
    let t3 = response.transmit_unix_ns;
    let err = response.server_error_ns() as i128;

    let lo = t3 - t4 - err;
    let hi = t2 - t1 + err;
    let at = HostTime::from_ticks(sent.ticks() / 2 + received.ticks() / 2);
    let sample = Sample::new(source, at, lo, hi).ok_or(NtpError::InvertedInterval)?;

    Ok(Exchange {
        sample,
        response,
        sent,
        received,
        round_trip: received.saturating_duration_since(sent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: i128 = 1_000_000_000;

    #[test]
    fn timestamp_unix_epoch_roundtrip() {
        // 2026-09-16T00:00:00Z ≈ 1_789_516_800
        let unix = 1_789_516_800 * NS + 123_456_789;
        let ts = NtpTimestamp::from_unix_ns(unix);
        let back = ts.to_unix_ns();
        // 32 位元小數的解析度約 0.23 ns。
        assert!((back - unix).abs() <= 1, "back={back} unix={unix}");
    }

    #[test]
    fn timestamp_known_vector() {
        // 1970-01-01T00:00:00Z 在 NTP 是 2208988800 秒、小數 0。
        let ts = NtpTimestamp((NTP_UNIX_OFFSET_SECS) << 32);
        assert_eq!(ts.to_unix_ns(), 0);
        // 加半秒：小數 0x8000_0000。
        let ts = NtpTimestamp((NTP_UNIX_OFFSET_SECS << 32) | 0x8000_0000);
        assert_eq!(ts.to_unix_ns(), 500_000_000);
    }

    #[test]
    fn timestamp_era_1_after_2036() {
        // era 1 的秒數 0 對應 2036-02-07T06:28:16Z ＝ Unix 2_085_978_496。
        let ts = NtpTimestamp(0 << 32);
        assert_eq!(ts.to_unix_ns(), 2_085_978_496 * NS);
    }

    #[test]
    fn request_encoding() {
        let b = NtpRequest { nonce: 0x1122_3344_5566_7788 }.encode();
        assert_eq!(b.len(), 48);
        assert_eq!(b[0], 0x23);
        assert_eq!(&b[40..48], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert!(b[1..40].iter().all(|&x| x == 0));
    }

    fn server_packet(nonce: u64, t2_unix: i128, t3_unix: i128) -> [u8; 48] {
        let mut b = [0u8; 48];
        b[0] = 0b00_100_100; // LI=0 VN=4 Mode=4
        b[1] = 2; // stratum 2
        b[3] = 0xE9u8; // precision −23
        b[4..8].copy_from_slice(&0x0000_0100u32.to_be_bytes()); // root delay 1/256 s ≈ 3.9 ms
        b[8..12].copy_from_slice(&0x0000_0080u32.to_be_bytes()); // root disp 1/512 s ≈ 1.95 ms
        b[12..16].copy_from_slice(&[10, 0, 0, 1]);
        b[24..32].copy_from_slice(&nonce.to_be_bytes());
        b[32..40].copy_from_slice(&NtpTimestamp::from_unix_ns(t2_unix).to_be_bytes());
        b[40..48].copy_from_slice(&NtpTimestamp::from_unix_ns(t3_unix).to_be_bytes());
        b
    }

    #[test]
    fn response_parse_fields() {
        let nonce = 42;
        let t2 = 1_789_516_800 * NS;
        let t3 = t2 + 50_000;
        let r = NtpResponse::parse(&server_packet(nonce, t2, t3)).unwrap();
        assert_eq!(r.version, 4);
        assert_eq!(r.stratum, 2);
        assert_eq!(r.precision, -23);
        assert_eq!(r.root_delay_ns, 3_906_250);
        assert_eq!(r.root_dispersion_ns, 1_953_125);
        assert_eq!(r.reference_id_string(), "10.0.0.1");
        assert_eq!(r.origin.0, nonce);
        assert!((r.receive_unix_ns - t2).abs() <= 1);
        assert!((r.transmit_unix_ns - t3).abs() <= 1);
        assert_eq!(r.server_error_ns(), 1_953_125 + 3_906_250 / 2);
    }

    #[test]
    fn response_rejections() {
        let good = server_packet(1, NS, NS + 1);
        assert_eq!(NtpResponse::parse(&good[..47]), Err(NtpError::TooShort(47)));

        let mut b = good;
        b[0] = 0b00_010_100; // VN=2
        assert_eq!(NtpResponse::parse(&b), Err(NtpError::BadVersion(2)));

        let mut b = good;
        b[0] = 0b00_100_011; // mode 3
        assert_eq!(NtpResponse::parse(&b), Err(NtpError::BadMode(3)));

        let mut b = good;
        b[0] = 0b11_100_100; // LI=3
        assert_eq!(NtpResponse::parse(&b), Err(NtpError::Unsynchronized));

        let mut b = good;
        b[1] = 0;
        b[12..16].copy_from_slice(b"RATE");
        assert_eq!(NtpResponse::parse(&b), Err(NtpError::KissOfDeath("RATE".into())));

        let mut b = good;
        b[1] = 16;
        assert_eq!(NtpResponse::parse(&b), Err(NtpError::BadStratum(16)));

        let mut b = good;
        b[32..40].fill(0);
        assert_eq!(NtpResponse::parse(&b), Err(NtpError::ZeroTimestamp));
    }

    #[test]
    fn sample_interval_from_four_timestamps() {
        // 本機：t₁ = 100 ms、t₄ = 140 ms（開機起）；往返 40 ms。
        // 遠端：t₂ = U + 1.020 s、t₃ = U + 1.022 s；處理 2 ms。
        // 真實偏移（遠端 − 本機）落在 [t₃ − t₄, t₂ − t₁] = [U + 0.882, U + 0.920]，寬 38 ms。
        let nonce = 7;
        let u = 1_789_516_800 * NS;
        let sent = HostTime::from_nanos(100_000_000);
        let received = HostTime::from_nanos(140_000_000);
        let t2 = u + 1_020_000_000;
        let t3 = u + 1_022_000_000;
        let r = NtpResponse::parse(&server_packet(nonce, t2, t3)).unwrap();
        let err = r.server_error_ns() as i128;

        let ex = sample_from_exchange(SourceKind::Standard, nonce, sent, received, r).unwrap();
        let s = ex.sample;
        let t1 = sent.as_nanos() as i128;
        let t4 = received.as_nanos() as i128;
        assert!((s.lo_ns - (t3 - t4 - err)).abs() <= 1);
        assert!((s.hi_ns - (t2 - t1 + err)).abs() <= 1);
        assert!(s.lo_ns < s.hi_ns);
        assert_eq!(ex.round_trip, Duration::from_millis(40));
        assert_eq!(s.at, HostTime::from_ticks(sent.ticks() / 2 + received.ticks() / 2));
    }

    #[test]
    fn sample_rejects_origin_mismatch_and_inverted() {
        let u = 1_789_516_800 * NS;
        let sent = HostTime::from_nanos(100_000_000);
        let received = HostTime::from_nanos(140_000_000);
        let r = NtpResponse::parse(&server_packet(7, u, u + 1)).unwrap();
        assert_eq!(
            sample_from_exchange(SourceKind::Standard, 8, sent, received, r).unwrap_err(),
            NtpError::OriginMismatch
        );

        // 伺服器處理 100 ms 但往返只有 40 ms：不可能，區間反轉。
        let r = NtpResponse::parse(&server_packet(7, u, u + 100_000_000)).unwrap();
        // server_error 會把區間往外撐約 3.9 ms，所以處理時間要明顯大於往返才會反轉。
        assert_eq!(
            sample_from_exchange(SourceKind::Standard, 7, sent, received, r).unwrap_err(),
            NtpError::InvertedInterval
        );
    }
}
