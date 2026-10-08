//! `battle_replay` — 离线校准工具：把抓包喂给解析器，并自动搜索最优 profile。
//!
//! 这是"让雷达真正出数据"那一步的主力工具（docs/PROTOCOL.md §2 的第 2–4 步）。
//! 它不依赖网络、不依赖引擎的异步流水，只调用纯粹的解析模块，因此可以在任何
//! 机器上跑（PC 也行），把参数调到最优后再上车。
//!
//! ```text
//! # 1) 用当前 profile 跑一遍，看 gate 通过率与通道直方图
//! cargo run --bin battle_replay -- --capture capture.ndjson
//!
//! # 2) 自动搜索最优 profile（会打印一张表，并给出推荐组合）
//! cargo run --bin battle_replay -- --capture capture.ndjson --sweep
//!
//! # 3) 用指定参数再跑，并顺带解属性（需要 channel_map.json）
//! cargo run --bin battle_replay -- --capture capture.ndjson \
//!     --packet-id-bits 10 --bunch-variant ue_classic --entities
//! ```
//!
//! 抓包文件即"协议诊断采集"落盘的 NDJSON（`battle-full-capture-*.ndjson`），
//! 每行形如 `{"ts_ms":…,"dir":"c2s","src":"h:…","len":…,"hex":"…"}`（端点已匿名化）。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use battle_proxy::battle::codec::bitstream::BitReader;
use battle_proxy::battle::codec::character::{
    read_property_block, CharacterCodecProfile, HandleTable,
};
use battle_proxy::battle::transport_crypto::{TransportKeys, decode_packet};
use battle_proxy::battle::udpxin::{BunchHeaderVariant, PacketFramer, ProtocolProfile};
use battle_proxy::battle::udpxin_entity::EntityTable;

/// 一行抓包记录里我们关心的字段。
#[derive(Debug, Clone)]
struct CaptureLine {
    dir: String,
    ts_ms: u64,
    len: usize,
    payload: Vec<u8>,
}

fn parse_hex(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let mut out = Vec::with_capacity(clean.len() / 2);
    let bytes = clean.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (bytes[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

fn load_capture(path: &PathBuf, limit: usize) -> std::io::Result<Vec<CaptureLine>> {
    let f = std::fs::File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
        let hex = match v.get("hex").and_then(|h| h.as_str()) {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };
        let payload = parse_hex(hex);
        if payload.is_empty() {
            continue;
        }
        out.push(CaptureLine {
            dir: v.get("dir").and_then(|d| d.as_str()).unwrap_or("?").to_string(),
            ts_ms: v.get("ts_ms").and_then(|t| t.as_u64()).unwrap_or(0),
            len: v.get("len").and_then(|l| l.as_u64()).unwrap_or(payload.len() as u64) as usize,
            payload,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// 跑一遍给定 profile，返回统计。
#[derive(Debug, Default, Clone)]
struct RunStats {
    total: usize,
    decoded_plain: usize,
    decoded_crypto: usize,
    gated: usize,
    bunches: usize,
    errors: BTreeMap<&'static str, usize>,
    channels: BTreeMap<u32, usize>,
    packet_ids_seen: usize,
}

fn run_profile(lines: &[CaptureLine], profile: &ProtocolProfile) -> RunStats {
    let mut st = RunStats::default();
    let keys = TransportKeys::default();

    for line in lines {
        st.total += 1;
        let framer = PacketFramer::new(profile.clone());
        let decoded = decode_packet(&line.payload, &keys, None, |b| framer.validate_datagram(b));
        if decoded.transformed {
            st.decoded_crypto += 1;
        } else {
            st.decoded_plain += 1;
        }

        let mut f = PacketFramer::new(profile.clone());
        let framed = f.frame(&decoded.bytes, line.ts_ms);
        if framed.gate_passed {
            st.gated += 1;
            st.packet_ids_seen += 1;
        }
        st.bunches += framed.bunches.len();
        for b in &framed.bunches {
            *st.channels.entry(b.channel_index).or_insert(0) += 1;
        }
        for e in &framed.errors {
            let label: &'static str = match e {
                battle_proxy::battle::udpxin::FrameError::InvalidPacketFraming => {
                    "invalid_packet_framing"
                }
                battle_proxy::battle::udpxin::FrameError::UnresolvedBunchHeaderVariant => {
                    "unresolved_bunch_header_variant"
                }
                battle_proxy::battle::udpxin::FrameError::BunchPayloadExceedsPacket { .. } => {
                    "bunch_payload_exceeds_packet"
                }
                battle_proxy::battle::udpxin::FrameError::TruncatedBunch => "truncated_bunch",
                battle_proxy::battle::udpxin::FrameError::BitOverflow => "bit_overflow",
            };
            *st.errors.entry(label).or_insert(0) += 1;
        }
    }
    st
}

fn report(label: &str, st: &RunStats) {
    let pct = |n: usize| if st.total == 0 { 0.0 } else { 100.0 * n as f64 / st.total as f64 };
    println!("── {label}");
    println!(
        "   报文 {}  明文/未识别 {}  加密 {}({:.1}%)  gate 通过 {}({:.1}%)  bunch {}",
        st.total,
        st.decoded_plain,
        st.decoded_crypto,
        pct(st.decoded_crypto),
        st.gated,
        pct(st.gated),
        st.bunches
    );
    if !st.errors.is_empty() {
        let mut errs: Vec<_> = st.errors.iter().collect();
        errs.sort_by(|a, b| b.1.cmp(a.1));
        let text: Vec<String> = errs.iter().map(|(k, v)| format!("{k}×{v}")).collect();
        println!("   错误：{}", text.join("  "));
    }
    if !st.channels.is_empty() {
        let mut ch: Vec<_> = st.channels.iter().collect();
        ch.sort_by(|a, b| b.1.cmp(a.1));
        let text: Vec<String> =
            ch.iter().take(12).map(|(k, v)| format!("ch{k}×{v}")).collect();
        println!("   通道直方图：{}", text.join("  "));
    }
}

/// 按最优 profile 解属性，打印实体表快照。
fn entities_report(lines: &[CaptureLine], profile: &ProtocolProfile, channel_map: Option<&str>) {
    let mut entities = EntityTable::new();
    let mut handles = HandleTable::new();
    if let Some(json) = channel_map {
        match entities.load_static_channel_map(json) {
            Ok(n) => println!("\nchannel_map：{n} 个通道"),
            Err(e) => println!("\nchannel_map 载入失败：{e}"),
        }
        match handles.load_channel_map_json(json) {
            Ok(n) => println!("handle 名字表：{n} 条属性"),
            Err(e) => println!("handle 名字表载入失败：{e}"),
        }
    }
    let cprofile = CharacterCodecProfile::default();

    let mut blocks = 0usize;
    let mut fields = 0usize;
    let mut with_pos = 0usize;
    for line in lines {
        let mut f = PacketFramer::new(profile.clone());
        let framed = f.frame(&line.payload, line.ts_ms);
        for b in &framed.bunches {
            if b.payload.is_empty() {
                continue;
            }
            let class = entities
                .class_for_channel(b.channel_index)
                .unwrap_or("BP_DFMCharacter_C")
                .to_string();
            let mut r = BitReader::new(&b.payload);
            let (block, _hot) = read_property_block(&mut r, &handles, &class, false, &cprofile);
            if !block.fields.is_empty() {
                blocks += 1;
                fields += block.fields.len();
                let rec = entities.open_channel(b.channel_index, Some(&class), None, line.ts_ms);
                rec.last_update_ms = line.ts_ms;
                if rec.has_valid_position() {
                    with_pos += 1;
                }
            }
        }
    }
    println!(
        "\n属性块 {blocks} 个，字段 {fields} 条，可绘制实体 {} 个（有位置 {with_pos}）",
        entities.drawable().count()
    );
    println!("（注意：位移解码需要正确解析 `ReplicatedMovement`，若这里为 0，先调 RepMovementProfile）");
}

#[allow(clippy::too_many_arguments)]
fn main() {
    let mut args = std::env::args().skip(1);
    let mut capture: Option<PathBuf> = None;
    let mut sweep = false;
    let mut entities = false;
    let mut limit = 20_000usize;
    let mut packet_id_bits = 0u32;
    let mut channel_index_bits = 0u32;
    let mut bunch_variant = BunchHeaderVariant::Ue5ReplicationPaused;
    let mut max_channels = 4096u32;
    let mut channel_map: Option<PathBuf> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--capture" => capture = args.next().map(PathBuf::from),
            "--sweep" => sweep = true,
            "--entities" => entities = true,
            "--limit" => limit = args.next().and_then(|v| v.parse().ok()).unwrap_or(limit),
            "--packet-id-bits" => packet_id_bits = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--channel-index-bits" => {
                channel_index_bits = args.next().and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            "--max-channels" => max_channels = args.next().and_then(|v| v.parse().ok()).unwrap_or(4096),
            "--bunch-variant" => {
                bunch_variant = match args.next().as_deref() {
                    Some("ue_classic") => BunchHeaderVariant::UeClassic,
                    Some("ue5") | Some("ue5_replication_paused") => {
                        BunchHeaderVariant::Ue5ReplicationPaused
                    }
                    _ => BunchHeaderVariant::Unresolved,
                }
            }
            "--channel-map" => channel_map = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "用法: battle_replay --capture <ndjson> [--sweep] [--entities]\n\
                     \x20 --limit N            最多读多少条（默认 20000）\n\
                     \x20 --packet-id-bits N   0 = SerializeInt(packet_id_max)\n\
                     \x20 --channel-index-bits N\n\
                     \x20 --max-channels N\n\
                     \x20 --bunch-variant ue_classic|ue5|unresolved\n\
                     \x20 --channel-map <channel_map.json>"
                );
                return;
            }
            other => eprintln!("忽略未知参数：{other}"),
        }
    }

    let Some(capture) = capture else {
        eprintln!("必须给 --capture <ndjson>（--help 看用法）");
        std::process::exit(2);
    };

    let lines = match load_capture(&capture, limit) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("读不了 {}：{e}", capture.display());
            std::process::exit(1);
        }
    };
    if lines.is_empty() {
        eprintln!("{} 里没有可解析的记录（需要每行含非空 \"hex\"）", capture.display());
        std::process::exit(1);
    }
    let c2s = lines.iter().filter(|l| l.dir == "c2s").count();
    println!(
        "抓包 {} 条（c2s {} / s2c {}），字节合计 {}",
        lines.len(),
        c2s,
        lines.len() - c2s,
        lines.iter().map(|l| l.len).sum::<usize>()
    );

    if sweep {
        // 逐维搜索：先定 packet_id_bits + bunch_variant，再看 channel_index_bits。
        println!("\n== profile 搜索（按 gate 通过率排序）==");
        let mut best: Option<(f64, ProtocolProfile)> = None;
        let mut rows: Vec<(f64, String, ProtocolProfile)> = Vec::new();
        for &pid_bits in &[0u32, 8, 10, 12, 16] {
            for &variant in &[BunchHeaderVariant::UeClassic, BunchHeaderVariant::Ue5ReplicationPaused] {
                for &ci_bits in &[0u32, 10, 12, 16] {
                    let profile = ProtocolProfile {
                        name: format!("pid{pid_bits}-{variant:?}-ci{ci_bits}"),
                        packet_id_bits: pid_bits,
                        packet_id_max: if pid_bits == 0 { 1023 } else { 0 },
                        channel_index_bits: ci_bits,
                        max_channels,
                        bunch_variant: variant,
                        ..ProtocolProfile::dfm_r39()
                    };
                    let st = run_profile(&lines, &profile);
                    let rate = st.gated as f64 / st.total.max(1) as f64;
                    let label = format!(
                        "packet_id_bits={pid_bits:<2} variant={variant:?} channel_index_bits={ci_bits:<2}"
                    );
                    rows.push((rate, label, profile.clone()));
                    if best.as_ref().map(|(r, _)| rate > *r).unwrap_or(true) {
                        best = Some((rate, profile));
                    }
                }
            }
        }
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (rate, label, _) in rows.iter().take(12) {
            println!("  {:6.1}%  {label}", rate * 100.0);
        }
        if let Some((rate, profile)) = best {
            println!(
                "\n推荐档：gate {:.1}%  packet_id_bits={} bunch_variant={:?} channel_index_bits={}",
                rate * 100.0,
                profile.packet_id_bits,
                profile.bunch_variant,
                profile.channel_index_bits
            );
            println!("（照着改 ProtocolProfile::dfm_r39()，或把它写进配置后重跑本工具确认）");
            let st = run_profile(&lines, &profile);
            report("推荐档明细", &st);
            if entities {
                let cm = channel_map.map(|p| std::fs::read_to_string(p).unwrap_or_default());
                entities_report(&lines, &profile, cm.as_deref());
            }
        }
        return;
    }

    let profile = ProtocolProfile {
        packet_id_bits,
        packet_id_max: if packet_id_bits == 0 { 1023 } else { 0 },
        channel_index_bits,
        max_channels,
        bunch_variant,
        ..ProtocolProfile::dfm_r39()
    };
    let stats = run_profile(&lines, &profile);
    report(
        &format!(
            "packet_id_bits={packet_id_bits} variant={bunch_variant:?} channel_index_bits={channel_index_bits}",
        ),
        &stats,
    );

    let gate_rate = stats.gated as f64 / stats.total.max(1) as f64;
    if gate_rate < 0.05 {
        println!(
            "\ngate 通过率只有 {:.1}%：说明分帧档不对。先跑 --sweep 找组合。",
            gate_rate * 100.0
        );
    } else if stats.channels.is_empty() {
        println!("\n有 bunch 但没解析出通道：检查 bunch 头变体与 channel_index 位宽。");
    } else {
        println!(
            "\n分帧基本可用（通道 {} 个）。下一步：确认属性块能解出位移（--entities），\
             再把参数写回 ProtocolProfile。",
            stats.channels.len()
        );
    }

    if entities {
        let cm = channel_map.map(|p| std::fs::read_to_string(p).unwrap_or_default());
        entities_report(&lines, &profile, cm.as_deref());
    }
}
