//! UDP socket 调优（需求 3）：把中继 socket 的收发缓冲拉到 1 MB 量级。
//!
//! # 为什么需要
//!
//! `per_association_ephemeral` 下每个 UDP 关联都自带一个临时 socket，而 iOS/Windows
//! 的默认收发缓冲都只有几十 KB。并发流一多（游戏复制流 + DNS + 其它 App 走同一个代理），
//! 接收缓冲一满内核就**直接丢包**，而且丢得毫无痕迹：转发代码只会看到"recv 没数据"，
//! 真机症状正是"UDP 流多了以后 DNS 解析超时、其他 App 像断网"。
//!
//! # 怎么调
//!
//! 直接用 `setsockopt(SO_RCVBUF/SO_SNDBUF)` —— `libc` 本来就在依赖里（见 Cargo.toml），
//! iOS/macOS/Windows 三边都有；为此再引第三方 crate 不值得。`libc` 的 windows 模块没有
//! 导出这两个常量（winsock2 里是固定值），所以在下面单独补一份。
//!
//! 上限 4 MiB / 下限 64 KiB 是刻意的：iOS 内核会把请求夹到它能给的最大值，而写 0 或
//! 极小值等于把缓冲调没了。
//!
//! # 失败必须静默降级
//!
//! 缓冲设置失败只是**性能**问题（回到默认缓冲），绝不能因此让关联建立失败。

use tokio::net::UdpSocket;
use tracing::debug;

/// 允许的最小请求值（低于它等于没调，还可能把缓冲调小）。
pub const MIN_UDP_SOCKET_BUFFER_BYTES: usize = 64 * 1024;
/// 允许的最大请求值：iOS 上通常能到 1–4 MB，再大内核也不会给。
pub const MAX_UDP_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// 把配置里的数值夹到 `[MIN, MAX]`；0（配置没写 / 显式关闭）保持 0 表示"不设置"。
pub fn clamp_buffer_bytes(requested: usize) -> usize {
    if requested == 0 {
        return 0;
    }
    requested.clamp(MIN_UDP_SOCKET_BUFFER_BYTES, MAX_UDP_SOCKET_BUFFER_BYTES)
}

#[cfg(unix)]
mod opt {
    //! BSD/Linux/iOS 的常量直接取 libc。
    pub const LEVEL: libc::c_int = libc::SOL_SOCKET;
    pub const RECV: libc::c_int = libc::SO_RCVBUF;
    pub const SEND: libc::c_int = libc::SO_SNDBUF;
}

#[cfg(windows)]
mod opt {
    //! libc 的 windows 模块不导出这三个常量；winsock2 里的值是固定的。
    pub const LEVEL: libc::c_int = 0xffff; // SOL_SOCKET
    pub const RECV: libc::c_int = 0x1002; // SO_RCVBUF
    pub const SEND: libc::c_int = 0x1001; // SO_SNDBUF
}

/// 裸 socket 句柄类型（unix: fd / windows: SOCKET）。
#[cfg(unix)]
type RawHandle = libc::c_int;
#[cfg(windows)]
type RawHandle = libc::SOCKET;

#[cfg(unix)]
fn raw_handle(socket: &UdpSocket) -> RawHandle {
    use std::os::unix::io::AsRawFd;
    socket.as_raw_fd()
}

#[cfg(windows)]
fn raw_handle(socket: &UdpSocket) -> RawHandle {
    use std::os::windows::io::AsRawSocket;
    socket.as_raw_socket() as libc::SOCKET
}

/// 写一个 SO_*BUF。返回 false 表示内核拒绝（调用方静默降级）。
#[cfg(unix)]
fn set_sockbuf(handle: RawHandle, option: libc::c_int, bytes: usize) -> bool {
    let value = bytes.min(libc::c_int::MAX as usize) as libc::c_int;
    // SAFETY: 句柄来自活的 tokio socket；optval 指向本栈帧里的 c_int，长度给的是
    // `size_of::<c_int>()`，符合 setsockopt 的约定。
    let rc = unsafe {
        libc::setsockopt(
            handle,
            opt::LEVEL,
            option,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    rc == 0
}

/// 写一个 SO_*BUF。返回 false 表示内核拒绝（调用方静默降级）。
#[cfg(windows)]
fn set_sockbuf(handle: RawHandle, option: libc::c_int, bytes: usize) -> bool {
    let value = bytes.min(libc::c_int::MAX as usize) as libc::c_int;
    // SAFETY: 同 unix 分支；windows 的 optval 是 `*const c_char`。
    let rc = unsafe {
        libc::setsockopt(
            handle,
            opt::LEVEL,
            option,
            &value as *const libc::c_int as *const libc::c_char,
            std::mem::size_of::<libc::c_int>() as libc::c_int,
        )
    };
    rc == 0
}

/// 读回一个 SO_*BUF（读回来的是内核**实际**给的尺寸，各平台可能不一样）。
#[cfg(unix)]
fn get_sockbuf(handle: RawHandle, option: libc::c_int) -> Option<usize> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: 同上，输出缓冲是本栈帧里的 c_int 且长度已给出。
    let rc = unsafe {
        libc::getsockopt(
            handle,
            opt::LEVEL,
            option,
            &mut value as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && value > 0 {
        Some(value as usize)
    } else {
        None
    }
}

/// 读回一个 SO_*BUF（读回来的是内核**实际**给的尺寸，各平台可能不一样）。
#[cfg(windows)]
fn get_sockbuf(handle: RawHandle, option: libc::c_int) -> Option<usize> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::c_int;
    // SAFETY: 同 unix 分支；windows 的 optval 是 `*mut c_char`。
    let rc = unsafe {
        libc::getsockopt(
            handle,
            opt::LEVEL,
            option,
            &mut value as *mut libc::c_int as *mut libc::c_char,
            &mut len,
        )
    };
    if rc == 0 && value > 0 {
        Some(value as usize)
    } else {
        None
    }
}

/// 给一个 UDP socket 设置收发缓冲。返回内核实际给的 `(recv, send)`；完全失败返回 `None`。
///
/// 这个函数**从不**返回错误、从不 panic：缓冲调不上去就继续用默认值（静默降级），
/// 调用的地方（共享 SOCKS 端口 socket、每个关联的临时 socket）都不需要处理失败分支。
pub fn apply_udp_socket_buffers(socket: &UdpSocket, bytes: usize) -> Option<(usize, usize)> {
    let requested = clamp_buffer_bytes(bytes);
    if requested == 0 {
        return None;
    }

    let handle = raw_handle(socket);
    // 收发分别设置：有的平台（或受限的容器内核）只允许其中一个，另一个失败不该拖累这个。
    let recv_ok = set_sockbuf(handle, opt::RECV, requested);
    let send_ok = set_sockbuf(handle, opt::SEND, requested);
    if !recv_ok && !send_ok {
        debug!(
            requested,
            "UDP socket buffer tuning was refused by the kernel; keeping the OS default"
        );
        return None;
    }

    let recv = get_sockbuf(handle, opt::RECV).unwrap_or(0);
    let send = get_sockbuf(handle, opt::SEND).unwrap_or(0);
    debug!(requested, recv, send, "UDP socket buffers tuned");
    Some((recv, send))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_size_is_clamped_into_the_sane_window() {
        assert_eq!(clamp_buffer_bytes(0), 0, "0 = 不设置");
        assert_eq!(clamp_buffer_bytes(1), MIN_UDP_SOCKET_BUFFER_BYTES);
        assert_eq!(clamp_buffer_bytes(1024), MIN_UDP_SOCKET_BUFFER_BYTES);
        assert_eq!(clamp_buffer_bytes(1 << 20), 1 << 20, "1 MiB 是合法请求（默认值）");
        assert_eq!(clamp_buffer_bytes(usize::MAX), MAX_UDP_SOCKET_BUFFER_BYTES);
    }

    /// 真 socket 上跑一遍：**失败也必须只是返回 None**，不能 panic、不能影响 socket 可用性。
    #[tokio::test]
    async fn buffers_are_applied_or_silently_degraded() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind udp");

        match apply_udp_socket_buffers(&socket, 1 << 20) {
            Some((recv, send)) => {
                // 读回来的必须是内核真实尺寸：不可能是 0。
                assert!(recv > 0, "recv buffer 读回 0");
                assert!(send > 0, "send buffer 读回 0");
            }
            None => {
                // 受限环境（容器/极小内核内存）允许拒绝：这就是"静默降级"。
            }
        }

        // 0 表示不设置：直接短路，不碰 socket。
        assert_eq!(apply_udp_socket_buffers(&socket, 0), None);

        // 调优之后 socket 仍然可用。
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        socket.send_to(b"ok", peer_addr).await.expect("send after tuning");
        let mut buf = [0u8; 8];
        let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(3), peer.recv_from(&mut buf))
            .await
            .expect("3 秒内应收到数据报")
            .expect("recv_from");
        assert_eq!(&buf[..n], b"ok");
    }
}
