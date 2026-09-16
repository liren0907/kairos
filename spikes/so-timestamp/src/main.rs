//! Spike A：macOS 上 `SO_TIMESTAMP_MONOTONIC` 給的核心接收時間戳，
//! 是否與 `mach_absolute_time` 同基準、同單位。
//!
//! 做法：收一個 UDP 封包，同時取得核心 cmsg 時間戳與 `recvmsg` 返回後
//! 立刻讀的 `mach_absolute_time`，比較兩者。先跑 loopback，再打一台真實的
//! NTP 伺服器確認真實網卡路徑上 cmsg 一樣會來。
//!
//! 執行：`cargo run -p spike-so-timestamp`

use std::ffi::CString;
use std::io;
use std::mem;
use std::ptr;
use std::thread;
use std::time::Duration;

/// SDK 的 sys/socket.h 有定義（`SCM_TIMESTAMP_MONOTONIC 0x04`，資料為 uint64_t），
/// 但 libc crate 尚未收錄。
const SCM_TIMESTAMP_MONOTONIC: libc::c_int = 0x04;

const NTP_HOST: &str = "time.stdtime.gov.tw";
const NTP_PORT: &str = "123";
const LOOPBACK_ROUNDS: usize = 20;
const NTP_ROUNDS: usize = 5;

// ---------------------------------------------------------------------------
// mach 時間基準
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    fn read() -> Self {
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: 傳入合法的可寫指標。
        let rc = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        assert_eq!(rc, 0, "mach_timebase_info 失敗");
        Self {
            numer: info.numer,
            denom: info.denom,
        }
    }

    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        (ticks as u128 * self.numer as u128 / self.denom as u128) as u64
    }

    fn ticks_to_us_f64(&self, ticks: i128) -> f64 {
        ticks as f64 * self.numer as f64 / self.denom as f64 / 1_000.0
    }
}

fn now_ticks() -> u64 {
    // SAFETY: 無參數、無副作用的系統呼叫。
    unsafe { mach2::mach_time::mach_absolute_time() }
}

// ---------------------------------------------------------------------------
// socket 與 recvmsg
// ---------------------------------------------------------------------------

/// 一次接收的結果：核心時間戳（cmsg 沒來就是 None）與 recvmsg 返回後的使用者空間時間戳。
struct Recv {
    len: usize,
    kernel_ticks: Option<u64>,
    user_ticks: u64,
    ctrunc: bool,
}

fn udp_socket_with_monotonic_ts() -> io::Result<libc::c_int> {
    // SAFETY: 標準 socket 建立與選項設定，參數皆為合法值。
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let on: libc::c_int = 1;
        let rc = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMP_MONOTONIC,
            &on as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        if rc != 0 {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(io::Error::new(
                err.kind(),
                format!("setsockopt(SO_TIMESTAMP_MONOTONIC) 失敗：{err}"),
            ));
        }

        let tv = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        let rc = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        if rc != 0 {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }

        Ok(fd)
    }
}

fn recv_with_kernel_ts(fd: libc::c_int, buf: &mut [u8]) -> io::Result<Recv> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // 一個 u64 的 cmsg 只需要 CMSG_SPACE(8)，給 64 位元組綽綽有餘。
    let mut control = [0u8; 64];

    // SAFETY: msghdr 以零初始化後只填合法指標與長度；recvmsg 返回後
    // 只在 msg_controllen 範圍內走訪 cmsg。
    unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = control.len() as libc::socklen_t;

        let n = libc::recvmsg(fd, &mut msg, 0);
        let user_ticks = now_ticks();
        if n < 0 {
            return Err(io::Error::last_os_error());
        }

        let ctrunc = msg.msg_flags & libc::MSG_CTRUNC != 0;
        let mut kernel_ticks = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let hdr = &*cmsg;
            if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == SCM_TIMESTAMP_MONOTONIC {
                let data = libc::CMSG_DATA(cmsg) as *const u64;
                kernel_ticks = Some(ptr::read_unaligned(data));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }

        Ok(Recv {
            len: n as usize,
            kernel_ticks,
            user_ticks,
            ctrunc,
        })
    }
}

fn sendto(
    fd: libc::c_int,
    payload: &[u8],
    addr: &libc::sockaddr_storage,
    addr_len: libc::socklen_t,
) -> io::Result<()> {
    // SAFETY: 位址結構由呼叫端填妥，長度一致。
    let n = unsafe {
        libc::sendto(
            fd,
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            0,
            addr as *const _ as *const libc::sockaddr,
            addr_len,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn resolve_ipv4(host: &str, port: &str) -> io::Result<(libc::sockaddr_storage, libc::socklen_t)> {
    let c_host = CString::new(host).unwrap();
    let c_port = CString::new(port).unwrap();
    // SAFETY: hints 零初始化後只填家族與型別；結果在複製後立即釋放。
    unsafe {
        let mut hints: libc::addrinfo = mem::zeroed();
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_DGRAM;
        let mut res: *mut libc::addrinfo = ptr::null_mut();
        let rc = libc::getaddrinfo(c_host.as_ptr(), c_port.as_ptr(), &hints, &mut res);
        if rc != 0 {
            return Err(io::Error::other(format!(
                "getaddrinfo({host}) 失敗，代碼 {rc}"
            )));
        }
        let ai = &*res;
        let mut storage: libc::sockaddr_storage = mem::zeroed();
        let len = ai.ai_addrlen;
        ptr::copy_nonoverlapping(
            ai.ai_addr as *const u8,
            &mut storage as *mut _ as *mut u8,
            len as usize,
        );
        libc::freeaddrinfo(res);
        Ok((storage, len))
    }
}

// ---------------------------------------------------------------------------
// 報表
// ---------------------------------------------------------------------------

struct Sample {
    kernel: Option<u64>,
    user: u64,
    ctrunc: bool,
}

fn report(title: &str, tb: Timebase, samples: &[Sample]) {
    println!("\n== {title} ==");
    let total = samples.len();
    let with_ts: Vec<&Sample> = samples.iter().filter(|s| s.kernel.is_some()).collect();
    let ctrunc = samples.iter().filter(|s| s.ctrunc).count();
    println!(
        "樣本數 {total}，帶 SCM_TIMESTAMP_MONOTONIC 的 {}，MSG_CTRUNC 的 {ctrunc}",
        with_ts.len()
    );

    if with_ts.is_empty() {
        println!("結論：cmsg 沒有出現，這條路徑拿不到核心時間戳。");
        return;
    }

    // 假設 A：核心給的是 mach tick，直接相減。
    let mut deltas_ticks: Vec<i128> = with_ts
        .iter()
        .map(|s| s.user as i128 - s.kernel.unwrap() as i128)
        .collect();
    deltas_ticks.sort_unstable();
    let (min, med, max) = (
        deltas_ticks[0],
        deltas_ticks[deltas_ticks.len() / 2],
        deltas_ticks[deltas_ticks.len() - 1],
    );
    println!(
        "假設「核心值為 mach tick」：user − kernel = min {min} / median {med} / max {max} tick，即 {:.1} / {:.1} / {:.1} µs",
        tb.ticks_to_us_f64(min),
        tb.ticks_to_us_f64(med),
        tb.ticks_to_us_f64(max),
    );

    // 假設 B：核心給的是奈秒，把 user 換成奈秒再相減。
    let mut deltas_ns: Vec<i128> = with_ts
        .iter()
        .map(|s| tb.ticks_to_ns(s.user) as i128 - s.kernel.unwrap() as i128)
        .collect();
    deltas_ns.sort_unstable();
    println!(
        "假設「核心值為奈秒」：user_ns − kernel = min {} / median {} / max {} ns",
        deltas_ns[0],
        deltas_ns[deltas_ns.len() / 2],
        deltas_ns[deltas_ns.len() - 1],
    );

    // 判讀：哪個假設下差值落在「小正數、小於 10 ms」的合理範圍。
    let plausible = |d: &[i128], upper: i128| d.iter().all(|&x| x >= 0 && x <= upper);
    let ten_ms_ticks = (10_000_000u128 * tb.denom as u128 / tb.numer as u128) as i128;
    let tick_ok = plausible(&deltas_ticks, ten_ms_ticks);
    let ns_ok = plausible(&deltas_ns, 10_000_000);
    let verdict = match (tick_ok, ns_ok) {
        (true, true) if tb.numer == tb.denom => "tick 即奈秒，兩個假設等價；同基準成立",
        (true, _) => "核心值為 mach tick，同基準、同單位",
        (false, true) => "核心值為奈秒，需經 timebase 換算後才與 mach tick 同基準",
        (false, false) => "兩個假設都不合理，需人工檢視上面的數字",
    };
    println!("判讀：{verdict}");
}

// ---------------------------------------------------------------------------
// 測試
// ---------------------------------------------------------------------------

fn loopback_test(tb: Timebase) -> io::Result<()> {
    let fd = udp_socket_with_monotonic_ts()?;
    // SAFETY: 綁定 127.0.0.1:0 後用 getsockname 取回實際埠號。
    let (addr, addr_len) = unsafe {
        let mut sin: libc::sockaddr_in = mem::zeroed();
        sin.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = 0;
        sin.sin_addr = libc::in_addr {
            s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be(),
        };
        let rc = libc::bind(
            fd,
            &sin as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage: libc::sockaddr_storage = mem::zeroed();
        let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let rc = libc::getsockname(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len);
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        (storage, len)
    };

    let payload = [0u8; 48];
    let mut buf = [0u8; 64];
    let mut samples = Vec::with_capacity(LOOPBACK_ROUNDS);
    for _ in 0..LOOPBACK_ROUNDS {
        sendto(fd, &payload, &addr, addr_len)?;
        let r = recv_with_kernel_ts(fd, &mut buf)?;
        samples.push(Sample {
            kernel: r.kernel_ticks,
            user: r.user_ticks,
            ctrunc: r.ctrunc,
        });
    }
    // SAFETY: fd 由本函式建立，關閉一次。
    unsafe { libc::close(fd) };
    report("Loopback（127.0.0.1 自送自收）", tb, &samples);
    Ok(())
}

fn ntp_test(tb: Timebase) -> io::Result<()> {
    let (addr, addr_len) = resolve_ipv4(NTP_HOST, NTP_PORT)?;
    let fd = udp_socket_with_monotonic_ts()?;

    // NTP mode 3 客戶端請求：LI=0、VN=4、Mode=3，其餘為零即可。
    let mut request = [0u8; 48];
    request[0] = 0x23;
    let mut buf = [0u8; 128];
    let mut samples = Vec::with_capacity(NTP_ROUNDS);

    println!("\n== 真實路徑（{NTP_HOST}:{NTP_PORT}）逐筆 ==");
    for i in 0..NTP_ROUNDS {
        let t_send = now_ticks();
        sendto(fd, &request, &addr, addr_len)?;
        match recv_with_kernel_ts(fd, &mut buf) {
            Ok(r) => {
                let rtt_user = r.user_ticks - t_send;
                let rtt_kernel = r.kernel_ticks.map(|k| k as i128 - t_send as i128);
                println!(
                    "#{i}: 回應 {} 位元組，cmsg {}，往返（user）{:.1} µs，往返（kernel）{}",
                    r.len,
                    if r.kernel_ticks.is_some() {
                        "有"
                    } else {
                        "無"
                    },
                    tb.ticks_to_us_f64(rtt_user as i128),
                    rtt_kernel
                        .map(|k| format!("{:.1} µs", tb.ticks_to_us_f64(k)))
                        .unwrap_or_else(|| "—".into()),
                );
                samples.push(Sample {
                    kernel: r.kernel_ticks,
                    user: r.user_ticks,
                    ctrunc: r.ctrunc,
                });
            }
            Err(e) => println!("#{i}: 接收失敗：{e}"),
        }
        if i + 1 < NTP_ROUNDS {
            thread::sleep(Duration::from_secs(1));
        }
    }
    // SAFETY: fd 由本函式建立，關閉一次。
    unsafe { libc::close(fd) };
    report(&format!("真實路徑（{NTP_HOST}）"), tb, &samples);
    Ok(())
}

fn main() {
    let tb = Timebase::read();
    println!(
        "mach_timebase_info: numer {} / denom {}（1 tick = {:.4} ns）",
        tb.numer,
        tb.denom,
        tb.numer as f64 / tb.denom as f64
    );

    if let Err(e) = loopback_test(tb) {
        println!("Loopback 測試失敗：{e}");
    }
    if let Err(e) = ntp_test(tb) {
        println!("真實路徑測試失敗：{e}");
    }
}
