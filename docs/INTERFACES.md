# MXrader 复刻工程 — 接口契约（唯一事实源）

所有子模块必须严格遵守本文件；任何不一致以本文件为准。

## 0. 目录约定

```
MXrader-DFM/
  core/    Rust crate 名 battle_proxy，产物 libbattle_proxy.a（staticlib）+ battle_proxy.h
  ios/     SwiftUI 壳工程 BattleReceiverOpen（Bundle ID com.mxrader.monstervision）
  web/     雷达前端（运行时资源，被 core 内嵌或本地目录加载）
  docs/    逆向报告 / 架构 / 协议
  scripts/ 构建、打包、重签名
```

## 1. C ABI（core/src/ios_bridge.rs 导出，Swift 直接调用）

```c
const char *battle_proxy_version(void);                     // "2.3.7-r39"
char       *battle_proxy_start(const char *config_json);     // 返回 status JSON，失败 phase=failed
char       *battle_proxy_stop(void);                         // 返回 status JSON
char       *battle_proxy_status(void);                       // 返回 status JSON
char       *battle_proxy_admin(const char *token, const char *path, const char *body);
void        battle_proxy_free_string(char *p);               // start/stop/status/admin 的返回值都由此释放
```

规则：
- 所有返回字符串均为堆分配、UTF-8、以 NUL 结尾，调用方必须用 `battle_proxy_free_string` 释放。
- `battle_proxy_start` 必须在后台线程解析，5 秒内返回 `phase=running` 或 `phase=failed`，不得阻塞主线程。
- 端口选择：在 `ports.range = [2025, 2045]` 中挑第一个可绑定的；`primary_port` 同时承载 TCP+UDP。

## 2. 配置 JSON（Swift → Rust，battle_proxy_start 入参）

```json
{
  "data_directory": "/var/mobile/Containers/Data/Application/<UUID>/Library/BattleReceiver",
  "brand": "mx",
  "endpoint": { "interface": "0.0.0.0", "ports": { "range": [2025, 2045] } },
  "transport": { "socks5": { "tcp_connect": true, "udp_associate": true, "udp_relay_mode": "per_association_ephemeral" },
                 "udp_nat_mapping": "per_client_endpoint_isolated" },
  "session_model": "one_port_one_player",
  "parser_async": true,
  "read_only_radar": true,
  "loot_parsing_enabled": true,
  "collection_policy": "enabled_only",
  "diagnostics": { "protocol_capture": false, "max_capture_mb": 64, "max_capture_seconds": 600 },
  "admin_token": "<random 32 hex>",
  "card": { "code": null, "activation_url": "https://<license-host>/api/activate" }
}
```

## 3. 状态 JSON（Rust → Swift）

```json
{
  "phase": "idle|preflight|starting|running|failed|stopping",
  "mode": "ios_receiver",
  "version": "2.3.7-r39",
  "socks_port": 2025,
  "web_port": 2025,
  "primary_port": 2025,
  "data_directory": "…",
  "web_session_token": "hex32",
  "endpoint": {
    "interface": "0.0.0.0",
    "display_address": "192.168.1.23",
    "radar_display_address": "http://192.168.1.23:2025/battle.html?brand=mx",
    "socks_url": "socks5://192.168.1.23:2025",
    "radar_url": "http://127.0.0.1:2025/battle.html?brand=mx"
  },
  "runtime_status": {
    "authorized": true,
    "card_tail": "9F2C",
    "failed_reason": null
  },
  "active_sessions": 1,
  "total_sessions": 3,
  "udp_packets_up": 0, "udp_packets_down": 0, "udp_invalid_packets": 0,
  "tcp_relay_failures": 0,
  "last_health_check": "2026-10-07T16:05:23Z",
  "error": null
}
```

## 4. HTTP 路由（axum，监听 web_port = primary_port）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | 302 → `/battle.html?brand=mx` |
| GET | `/battle.html` | 雷达页面（`web/index.html`） |
| GET | `/license` | 卡密激活页（内嵌，见 `core/assets/battle_card.html`） |
| POST | `/license` | **激活页自己用的就是这个**（原始 HTML 里是 `fetch('/license',{method:'POST'})`） |
| POST | `/license/activate` | 同上的别名，方便手写请求 |
| GET | `/license/status` | `{"authorized":true}` |
| GET | `/api/status` | 状态 JSON（带一次性 `web_session_token` 校验，本机豁免） |
| GET | `/api/socks5/hiddify.json` | Hiddify/sing-box 分享配置（socks5 out 出站 + udp） |
| GET | `/ws` | WebSocket，雷达状态流 |
| GET | `/download` | Hiddify 配置一键导入（返回 profile 文本） |
| POST | `/api/admin/*` | 仅本机 + `BATTLE_ADMIN_TOKEN`：`diag`、`loot`、`session/reset`、`announcement`、`capture/start|stop|download`、`shutdown` |

## 5. WebSocket 消息（server → 雷达前端）

```json
{ "type": "hello",  "brand": "mx", "map": "ZeroDam", "tile_template": "/tiles/{map}/{z}/{x}/{y}.png", "world": {"origin_x":0,"origin_y":0,"scale":0.01,"yaw_offset_deg":0} }
{ "type": "state",  "ts": 1699999999999, "tick": 4211, "self": { … }, "players": [ … ], "kills": [ … ], "loot": [ … ], "traces": [ … ] }
{ "type": "diag",   "counters": { "udp_packets_up":0, "udp_packets_down":0, "udp_invalid_packets":0, "active_sessions":1, "total_sessions":3, "loot_payloads_skipped":0 } }
{ "type": "bye" }
```

玩家条目字段（radar `players[]`，全部为 SI 单位：米 / 度）：

```json
{ "uuid":"…","name":"…","kind":"player|ai|deathbox|loot",
  "x":0.0,"y":0.0,"z":0.0,            // 世界坐标（米，Z 向上）
  "vx":0.0,"vy":0.0,"vz":0.0,
  "yaw":0.0,"pitch":0.0,"roll":0.0,   // 已做转向修正：yaw 0=正北，顺时针为正
  "team":1,"camp":2,"hp":100,"max_hp":100,
  "alive":true,"visible":true,"distance":42.7,
  "weapon":"…","hero_id":0,"level":0,"rank_score":0,
  "last_seen_ms":1699999999000, "source":"move|state|fire|kill" }
```

## 6. 前端契约（web/）

- 纯静态：`index.html` + `radar.js` + `style.css`，不得依赖 CDN（局域网无外网），Leaflet 本地内联。
- 就绪标记：`document.documentElement.dataset.battleReady = '1'`，且 `#app` 有子节点，且存在 `.leaflet-container`（iOS 壳用这三条做加载探针）。
- 地图投影：`world → map` 使用 `hello.world` 的 `origin_x/origin_y/scale/yaw_offset_deg`。
- 转向修正：`screenHeading = yaw + yaw_offset_deg`，箭头按 `screenHeading` 旋转；3D 透视模式下启用 `pitch/roll` 修正矩阵。
- 只读模式：收到 `read_only_radar=true` 时隐藏所有写操作按钮。
