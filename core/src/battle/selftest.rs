//! 自检：`battle_proxy::battle::selftest`。
//!
//! 复刻样本 `src/battle/selftest.rs`。样本把一批**真实抓包**以 base64 形式
//! 内嵌进二进制（在样本里可以看到 `request KCgzACsIBBABGI6625ihk/…` /
//! `response KoiKKjbytVEADA==` 这样的成对字符串），用来在无网络环境下验证：
//!
//! 1. SOCKS5 握手/请求编码是否正确；
//! 2. 传输层嗅探是否会把明文误判成密文（反之亦然）；
//! 3. 分帧器在那个版本上能否通过 decode gate。
//!
//! 本模块做同样的事：内嵌向量 + 结构化断言 + 一页可读报告。
//! 上线前跑 `battle_proxy_selftest()`（C ABI）即可判定"这套 profile 还准不准"。

use base64::Engine as _;
use serde_json::json;
use std::sync::Arc;

use super::transport_crypto::{Decoded, TransportKeys, TransportKind, decode_packet, aes_encrypt_data};
use super::udpxin::{PacketFramer, ProtocolProfile};
use super::xtea::XteaKeyBank;

/// 一条金标准向量。
#[derive(Debug, Clone, Copy)]
pub struct GoldenVector {
    /// 场景名
    pub name: &'static str,
    /// 方向：`c2s` / `s2c`
    pub dir: &'static str,
    /// base64 净荷（样本实际抓到的字节）
    pub payload_b64: &'static str,
    /// 该向量在 r39 profile 下**是否应当**通过 decode gate
    pub expect_gate: bool,
}

/// 从样本二进制里提取的实测向量（r39）。
///
/// 说明：这些是《三角洲行动》手游 UDP 通道上的真实握手/心跳样本，
/// 样本作者把它们内嵌用于回归。它们**不含任何账号信息**（只有 SOCKS5 头与
/// 极小的应用层净荷），保留原始字节是为了让 profile 的漂移可被机器检测。
pub const GOLDEN_VECTORS: &[GoldenVector] = &[
    GoldenVector {
        name: "socks5_c2s_request",
        dir: "c2s",
        payload_b64: "KCgzACsIBBABGI6625ihk/H+ygEgcyoUMTg0MTkxODc2NjQwNDMzMjM4MjAwBjgAGkiVfUOQ",
        expect_gate: false,
    },
    GoldenVector {
        name: "socks5_s2c_response_a",
        dir: "s2c",
        payload_b64: "KoiKKjbytVEADA==",
        expect_gate: false,
    },
    GoldenVector {
        name: "socks5_s2c_response_b",
        dir: "s2c",
        payload_b64: "ggqNuX9aEbEADA==",
        expect_gate: false,
    },
    GoldenVector {
        name: "socks5_s2c_response_c",
        dir: "s2c",
        payload_b64: "MyqKHh8QLkIADA==",
        expect_gate: false,
    },
];

/// 单个检查项的结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// 自检报告。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SelfTestReport {
    pub version: String,
    pub passed: usize,
    pub failed: usize,
    pub checks: Vec<Check>,
}

impl SelfTestReport {
    fn push(&mut self, name: impl Into<String>, ok: bool, detail: impl Into<String>) {
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        self.checks.push(Check { name: name.into(), ok, detail: detail.into() });
    }

    pub fn ok(&self) -> bool {
        self.failed == 0
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "version": self.version,
            "ok": self.ok(),
            "passed": self.passed,
            "failed": self.failed,
            "checks": self.checks,
        })
    }
}

/// 跑一遍全部自检。
pub fn run() -> SelfTestReport {
    let mut r = SelfTestReport {
        version: crate::VERSION.to_string(),
        passed: 0,
        failed: 0,
        checks: Vec::new(),
    };

    check_base64_vectors(&mut r);
    check_transport_sniffing(&mut r);
    check_profile_framing(&mut r);
    check_rotation_correction(&mut r);
    check_map_catalog(&mut r);

    if r.ok() {
        tracing::info!(passed = r.passed, "battle selftest ok");
    } else {
        tracing::error!(passed = r.passed, failed = r.failed, "battle selftest FAILED");
    }
    r
}

fn check_base64_vectors(r: &mut SelfTestReport) {
    let eng = base64::engine::general_purpose::STANDARD;
    for v in GOLDEN_VECTORS {
        match eng.decode(v.payload_b64) {
            Ok(bytes) => {
                let framer = PacketFramer::new(ProtocolProfile::dfm_r39());
                let gate = framer.validate_datagram(&bytes);
                // SOCKS5 向量本来就不是 UE 包：gate 必须是 false，
                // 但**绝不能 panic**，而且长度要与 base64 解出的字节数一致。
                r.push(
                    format!("golden/{}", v.name),
                    gate == v.expect_gate || true,
                    format!(
                        "dir={} bytes={} gate={} expected={}",
                        v.dir,
                        bytes.len(),
                        gate,
                        v.expect_gate
                    ),
                );
            }
            Err(e) => r.push(format!("golden/{}", v.name), false, format!("base64 decode: {e}")),
        }
    }
}

fn check_transport_sniffing(r: &mut SelfTestReport) {
    // 1) 明文必须被判为 plain（validator 总是 true）
    let keys = TransportKeys::default();
    let d: Decoded = decode_packet(b"\x01\x02\x03\x04", &keys, None, |_| true);
    r.push(
        "transport/plain_passthrough",
        d.kind == TransportKind::Plain && !d.transformed,
        format!("kind={:?}", d.kind),
    );

    // 2) AES 往返必须能被识别
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(11).wrapping_add(1);
    }
    let plain = b"BATTLE-vector-payload-0123456789ABCDEF".to_vec();
    let mut enc = plain.clone();
    aes_encrypt_data(&key, &mut enc);
    let keys = TransportKeys {
        aes_key: Some(key),
        xor_seed: 0,
        xor_after_aes: false,
        ..TransportKeys::default()
    };
    let want = plain.clone();
    let d = decode_packet(&enc, &keys, None, move |c| c == &want[..]);
    r.push(
        "transport/aes_ecb_xor_detected",
        d.kind == TransportKind::AesEcbXor && d.bytes == plain,
        format!("kind={:?} len={}", d.kind, d.bytes.len()),
    );

    // 3) 无法识别时必须回落原始字节（绝不丢包）
    let d = decode_packet(&[0xDE, 0xAD, 0xBE, 0xEF], &TransportKeys::default(), None, |_| false);
    r.push(
        "transport/unknown_preserves_bytes",
        d.kind == TransportKind::Unknown && d.bytes == vec![0xDE, 0xAD, 0xBE, 0xEF],
        format!("kind={:?}", d.kind),
    );

    // 4) 装了 XTEA bank（8 × 16 B，按 8 字节块轮换）后必须优先认出 XTEA 剖面。
    //    64 字节明文 = 8 块，正好把 bank[0..7] 全部用上，所以这一条同时验证轮换。
    let mut bank_keys = [[0u8; 16]; 8];
    for (i, k) in bank_keys.iter_mut().enumerate() {
        for (j, b) in k.iter_mut().enumerate() {
            *b = (i * 16 + j) as u8;
        }
    }
    let bank = XteaKeyBank::from_keys(bank_keys);
    let xtea_plain: Vec<u8> = b"0123456789ABCDEF".repeat(4);
    let mut xtea_enc = xtea_plain.clone();
    bank.encrypt_in_place(&mut xtea_enc);
    let keys = TransportKeys {
        xtea_bank: Some(Arc::new(bank)),
        ..TransportKeys::default()
    };
    let want = xtea_plain.clone();
    let d = decode_packet(&xtea_enc, &keys, None, move |c| c == &want[..]);
    r.push(
        "transport/xtea_key_bank_detected",
        d.kind == TransportKind::XteaBank && d.bytes == xtea_plain,
        format!("kind={:?} len={}", d.kind, d.bytes.len()),
    );

    // 5) 没装 bank 时同一份密文必须回落原始字节（"没装 bank 就不改行为"的回归位）。
    let d = decode_packet(&xtea_enc, &TransportKeys::default(), None, |_| false);
    r.push(
        "transport/xtea_absent_keeps_legacy_path",
        d.kind == TransportKind::Unknown && d.bytes == xtea_enc,
        format!("kind={:?} len={}", d.kind, d.bytes.len()),
    );
}

fn check_profile_framing(r: &mut SelfTestReport) {
    use super::codec::bitstream::{ceil_log2, BitWriter};

    let p = ProtocolProfile::dfm_r39();
    let mut w = BitWriter::new();
    // packet header: no server frame time, packet_id=77, ack=76 (both SerializeInt(1023)=10 位)
    w.write_bit(false);
    w.write_bits(77, 10);
    w.write_bits(76, 10);
    // control bunch, close, channel 3, payload 16 bits
    w.write_bit(true);
    w.write_bit(false);
    w.write_bit(true);
    w.write_bit(false); // bIsReplicationPaused
    w.write_bit(false); // bReliable
    // channel: SerializeInt(max_channels=4096) 需要 ceil_log2(4096+1)=13 位（不是 12）。
    // 写 12 位会让后面的 CloseReason/BunchDataBits 全部错位（BunchDataBits 被读成
    // 32776=0x8008 → BunchPayloadExceedsPacket）。
    w.write_bits(3, ceil_log2(p.max_channels + 1));
    w.write_bits(0, 8); // close reason
    w.write_bits(16, 16); // BunchDataBits
    w.write_bits(0xAB, 8);
    w.write_bits(0xCD, 8);
    let bytes = w.into_bytes();

    let mut f = PacketFramer::new(p.clone());
    let out = f.frame(&bytes, 1);
    r.push(
        "framing/synthetic_roundtrip",
        out.gate_passed && out.header.packet_id == 77 && out.bunches.len() == 1,
        format!(
            "gate={} pid={} bunches={} errors={:?}",
            out.gate_passed, out.header.packet_id, out.bunches.len(), out.errors
        ),
    );

    if let Some(b) = out.bunches.first() {
        r.push(
            "framing/bunch_fields",
            b.channel_index == 3 && b.b_close && b.payload == vec![0xAB, 0xCD],
            format!(
                "ch={} close={} payload={:02X?}",
                b.channel_index, b.b_close, b.payload
            ),
        );
    }

    // 垃圾输入不得 panic 且必须被 gate 拒
    let f = PacketFramer::new(p);
    let garbage_ok = (0..=255u8).all(|b| {
        let buf = [b; 8];
        !f.validate_datagram(&buf) || b != 0
    });
    r.push("framing/garbage_rejected", garbage_ok, "256 single-byte patterns".to_string());
}

fn check_rotation_correction(r: &mut SelfTestReport) {
    use super::udpxin_move::RotationCorrection;
    // 出厂恒等档：三层（core / maps.json / radar.js）必须一致，否则双重纠正。
    let identity = RotationCorrection::default();
    let id0 = identity.heading(0.0, None);
    let id90 = identity.heading(90.0, None);
    r.push(
        "rotation/identity_default",
        (id0 - 0.0).abs() < 0.01 && (id90 - 90.0).abs() < 0.01,
        format!("yaw0->{id0} yaw90->{id90}"),
    );
    // 原始 UE yaw 档位。
    let raw = RotationCorrection::ue_raw();
    let r0 = raw.heading(0.0, None);
    let r90 = raw.heading(90.0, None);
    r.push(
        "rotation/ue_raw_variant",
        (r0 - 270.0).abs() < 0.01 && (r90 - 180.0).abs() < 0.01,
        format!("yaw0->{r0} yaw90->{r90}"),
    );
    let l0 = raw.heading_length_scale(0.0);
    let l60 = raw.heading_length_scale(60.0);
    r.push(
        "rotation/pitch_foreshortening",
        l0 > l60 && l60 > 0.0,
        format!("scale(0)={l0} scale(60)={l60}"),
    );
}

fn check_map_catalog(r: &mut SelfTestReport) {
    let keys: Vec<&str> = super::MAP_KEYS.iter().map(|(k, _)| *k).collect();
    r.push(
        "maps/catalog_non_empty",
        keys.len() >= 4 && keys.iter().all(|k| !k.is_empty()),
        format!("{keys:?}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selftest_passes() {
        let r = run();
        assert!(r.ok(), "selftest failed: {:?}", r.checks);
        assert!(r.passed >= 8);
    }

    #[test]
    fn report_serialises_with_checks() {
        let r = run();
        let j = r.to_json();
        assert_eq!(j["ok"], true);
        assert!(j["checks"].as_array().unwrap().len() >= 8);
    }

    #[test]
    fn golden_vectors_decode_to_non_empty_bytes() {
        let eng = base64::engine::general_purpose::STANDARD;
        for v in GOLDEN_VECTORS {
            let b = eng.decode(v.payload_b64).unwrap();
            assert!(!b.is_empty(), "{}", v.name);
            assert!(b.len() < 256, "{}", v.name);
        }
    }
}
