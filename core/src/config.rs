//! 配置模型：对应 `battle_proxy::config`，是 Swift → Rust JSON 的 serde 映射。
//! 字段名与 docs/INTERFACES.md §2 完全一致，且与样本中可见的配置键保持同名
//! （`socks_port` / `primary_port` / `udp_relay_mode` / `session_model` /
//!  `loot_parsing_enabled` / `collection_policy` / `read_only_radar` …）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// UDP 中继模式（样本可见值：`per_association_ephemeral`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UdpRelayMode {
    /// 每个 UDP ASSOCIATE 分配一个临时端口（默认）。
    PerAssociationEphemeral,
    /// 所有 UDP 关联复用 SOCKS 端口。
    SharedPort,
}

/// UDP NAT 映射策略（样本可见值：`per_client_endpoint_isolated` /
/// `per_client_endpoint_separate_dual_stack`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UdpNatMapping {
    /// 每个客户端端点独立映射（默认，多流时不会串流）。
    PerClientEndpointIsolated,
    /// 双栈（v4/v6）分离映射。
    PerClientEndpointSeparateDualStack,
}

/// 采集策略（样本可见值：`enabled_only` / `retained_when_disabled`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionPolicy {
    /// 仅在解析开启时保留抓包。
    EnabledOnly,
    /// 解析关闭也保留（占盘更多）。
    RetainedWhenDisabled,
}

/// 会话模型（样本可见值：`one_port_one_player`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModel {
    /// 一个源端口视为一个玩家会话。
    OnePortOnePlayer,
    /// 一个 TCP 连接一个会话（调试用）。
    OneConnectionOneSession,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortsConfig {
    /// 候选端口区间，闭区间。默认 2025–2045。
    #[serde(default = "default_port_range")]
    pub range: [u16; 2],
}

fn default_port_range() -> [u16; 2] {
    [crate::DEFAULT_PORT_RANGE.0, crate::DEFAULT_PORT_RANGE.1]
}

impl Default for PortsConfig {
    fn default() -> Self {
        Self { range: default_port_range() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// 监听网卡，默认 0.0.0.0（局域网可达）。
    #[serde(default = "default_interface")]
    pub interface: String,
    #[serde(default)]
    pub ports: PortsConfig,
}

fn default_interface() -> String {
    "0.0.0.0".to_string()
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self { interface: default_interface(), ports: PortsConfig::default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Socks5TransportConfig {
    #[serde(default = "yes")]
    pub tcp_connect: bool,
    #[serde(default = "yes")]
    pub udp_associate: bool,
    /// 是否要求 UDP 与 TCP 同端口。
    #[serde(default)]
    pub udp_same_port: bool,
    /// 单方向突发上限（字节），超过则丢弃并计数，避免大包堆积。
    #[serde(default = "default_udp_burst")]
    pub udp_single_direction_burst: u32,
    #[serde(default = "default_udp_concurrent")]
    pub udp_concurrent_associations: u32,
    #[serde(default = "default_udp_relay_mode")]
    pub udp_relay_mode: UdpRelayMode,
}

fn yes() -> bool {
    true
}
fn default_udp_burst() -> u32 {
    512 * 1024
}
fn default_udp_concurrent() -> u32 {
    8
}
fn default_udp_relay_mode() -> UdpRelayMode {
    UdpRelayMode::PerAssociationEphemeral
}

impl Default for Socks5TransportConfig {
    fn default() -> Self {
        Self {
            tcp_connect: true,
            udp_associate: true,
            udp_same_port: false,
            udp_single_direction_burst: default_udp_burst(),
            udp_concurrent_associations: default_udp_concurrent(),
            udp_relay_mode: default_udp_relay_mode(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportConfig {
    #[serde(default)]
    pub socks5: Socks5TransportConfig,
    #[serde(default = "default_nat_mapping")]
    pub udp_nat_mapping: UdpNatMapping,
    /// 认证方式（样本可见键，当前仅 `none`）。
    #[serde(default = "default_auth")]
    pub authentication: String,
    /// 传输加密（样本可见键，当前 `none`：明文 socks5）。
    #[serde(default = "default_encryption")]
    pub encryption: String,
}

fn default_nat_mapping() -> UdpNatMapping {
    UdpNatMapping::PerClientEndpointIsolated
}

impl Default for UdpNatMapping {
    fn default() -> Self {
        default_nat_mapping()
    }
}

impl Default for UdpRelayMode {
    fn default() -> Self {
        default_udp_relay_mode()
    }
}
fn default_auth() -> String {
    "none".to_string()
}
fn default_encryption() -> String {
    "none".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsConfig {
    /// 协议诊断采集开关（/api/admin/capture/* 控制）。
    #[serde(default)]
    pub protocol_capture: bool,
    #[serde(default = "default_capture_mb")]
    pub max_capture_mb: u64,
    #[serde(default = "default_capture_secs")]
    pub max_capture_seconds: u64,
}

fn default_capture_mb() -> u64 {
    64
}
fn default_capture_secs() -> u64 {
    600
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            protocol_capture: false,
            max_capture_mb: default_capture_mb(),
            max_capture_seconds: default_capture_secs(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardConfig {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default = "default_activation_url")]
    pub activation_url: String,
}

fn default_activation_url() -> String {
    "https://license.invalid/api/activate".to_string()
}

/// 顶层配置。`Default` 用于 C ABI 收到 `null`/空字符串时的兜底。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_data_dir")]
    pub data_directory: PathBuf,
    #[serde(default = "default_brand")]
    pub brand: String,
    #[serde(default)]
    pub endpoint: EndpointConfig,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default = "default_session_model")]
    pub session_model: SessionModel,
    /// 异步解析队列（转发不等解析）。
    #[serde(default = "yes")]
    pub parser_async: bool,
    /// 雷达网页只读：远端不得触发重置/调试/解析开关。
    #[serde(default = "yes")]
    pub read_only_radar: bool,
    #[serde(default = "yes")]
    pub loot_parsing_enabled: bool,
    #[serde(default = "default_collection")]
    pub collection_policy: CollectionPolicy,
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    /// 管理接口令牌；仅本机可用，但令牌仍是第二道门。
    #[serde(default = "default_admin_token")]
    pub admin_token: String,
    /// 雷达前端静态目录的绝对路径。
    ///
    /// iOS 上**必须**由 Swift 侧传进来（app bundle 里的 `web/` 目录），因为设备上
    /// 没有 cargo 的源码目录、也没有当前工作目录这一说。传空则走
    /// `embed::web_root()` 的自动解析（PC 上够用）。
    #[serde(default)]
    pub web_root: Option<String>,
    #[serde(default)]
    pub card: CardConfig,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(".")
}
fn default_brand() -> String {
    crate::BRAND_VARIANT.to_string()
}
fn default_session_model() -> SessionModel {
    SessionModel::OnePortOnePlayer
}
fn default_collection() -> CollectionPolicy {
    CollectionPolicy::EnabledOnly
}

/// 生成 32 位 hex 随机令牌（无第三方 RNG 依赖时的兜底路径）。
fn default_admin_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_directory: default_data_dir(),
            brand: default_brand(),
            endpoint: EndpointConfig::default(),
            transport: TransportConfig::default(),
            session_model: default_session_model(),
            parser_async: true,
            read_only_radar: true,
            loot_parsing_enabled: true,
            collection_policy: default_collection(),
            diagnostics: DiagnosticsConfig::default(),
            admin_token: default_admin_token(),
            web_root: None,
            card: CardConfig::default(),
        }
    }
}

impl Config {
    /// 从 JSON 解析；空串 / `null` / `{}` 均回落到 `Default`。
    pub fn from_json_str(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "null" {
            return Ok(Self::default());
        }
        let cfg: Self = serde_json::from_str(trimmed)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let [lo, hi] = self.endpoint.ports.range;
        anyhow::ensure!(lo > 0 && hi >= lo, "invalid port range {lo}-{hi}");
        anyhow::ensure!(hi - lo <= 512, "port range too wide: {lo}-{hi}");
        Ok(())
    }

    /// 卡密 / 公告等落盘目录。
    pub fn ensure_data_directory(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.data_directory).map_err(|e| {
            anyhow::anyhow!("无法创建应用数据目录：{} ({e})", self.data_directory.display())
        })
    }

    pub fn store_path(&self, name: &str) -> PathBuf {
        self.data_directory.join(name)
    }

    /// 候选端口迭代器（含首尾）。
    pub fn candidate_ports(&self) -> impl Iterator<Item = u16> {
        let [lo, hi] = self.endpoint.ports.range;
        lo..=hi
    }

    pub fn is_loopback_only(&self) -> bool {
        self.endpoint.interface.starts_with("127.")
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_directory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_from_empty() {
        let c = Config::from_json_str("").unwrap();
        assert_eq!(c.endpoint.ports.range, [2025, 2045]);
        assert!(c.read_only_radar && c.parser_async);
        assert_eq!(c.brand, "mx");
    }

    #[test]
    fn unknown_fields_are_tolerated_but_required_ones_are_typed() {
        let raw = r#"{"data_directory":"/tmp/x","transport":{"socks5":{"udp_associate":true}}}"#;
        let c = Config::from_json_str(raw).unwrap();
        assert!(c.transport.socks5.udp_associate);
        assert!(c.transport.socks5.tcp_connect, "serde default must fill tcp_connect");
    }
}
