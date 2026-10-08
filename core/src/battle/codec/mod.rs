//! 复制属性编解码：`battle_proxy::battle::codec`。
//!
//! 复刻样本 `src/battle/codec/*`。子模块与样本一一对应：
//!
//! | 模块 | 样本里的职责 |
//! |---|---|
//! | `bitstream` | UE `FBitReader`/`FBitWriter` 原语 |
//! | `s2c` | 服务端→客户端的 RPC/属性消息分帧（`request_reference_bytes`） |
//! | `character` | `BP_DFMCharacter_C` 的 RepLayout 属性遍历 + 字段类型推断 |
//! | `fire` | 开火 RPC → 弹道还原（`battle_fire_cli`） |
//! | `killchain` | 击杀链（伤害类型枚举 + 击杀数组） |
//! | `container_collector` | `DFMContainerDataCollector` 物资容器 |
//! | `capture` | 原始采集环形缓冲（协议诊断落盘） |
//!
//! 所有子模块都遵守两条硬约束：
//! 1. **绝不 panic**：位流越界只会打标（`BitReader::overflowed`），调用方按
//!    "本包不可信"处理；
//! 2. **绝不阻塞网络路径**：纯计算，无锁、无 IO。

pub mod bitstream;
pub mod capture;
pub mod character;
pub mod container_collector;
pub mod fire;
pub mod killchain;
pub mod s2c;

pub use bitstream::{BitReader, BitWriter, ceil_log2, deg_to_rotator, rotator_to_deg};

/// 属性块解析的统一结果：命中的 handle → 原始值。
#[derive(Debug, Default, Clone)]
pub struct PropertyBlock {
    pub fields: Vec<DecodedField>,
    /// 消耗位数。
    pub bits_used: usize,
    /// 未对齐收尾（样本 `movement RepLayout not exactly closed`）。
    pub closure_ok: bool,
    pub notes: Vec<&'static str>,
}

/// 单个属性的解码结果。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedField {
    /// handle 号（channel_map.json 的 handles 键）。
    pub handle: u32,
    /// 属性名（由名字表给出）。
    pub name: String,
    pub value: FieldValue,
}

/// 属性值。只覆盖雷达需要的类型；未知类型保留原始位。
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Bool(bool),
    Int(i64),
    Float(f32),
    Vector(crate::battle::udpxin_entity::Vec3),
    Rotator(crate::battle::udpxin_entity::Rot3),
    String(String),
    /// 结构化但本项目不解释的类型（保留位偏移，供后续离线分析）。
    Raw { bit_start: usize, bit_len: usize },
}
