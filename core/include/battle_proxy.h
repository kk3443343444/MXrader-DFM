/*
 * battle_proxy.h — MXrader 接收器核心的 C ABI
 *
 * 契约见 docs/INTERFACES.md §1。Swift 侧通过 bridging header 引入本文件，
 * 只依赖下面 7 个符号；Rust 内部重构不影响 iOS 壳。
 *
 * 内存约定：
 *   - battle_proxy_version() 返回静态内存，**不需要**释放；
 *   - 其余返回 char* 的函数都是堆分配，调用方必须用 battle_proxy_free_string() 释放；
 *   - 传 NULL 给任何 const char* 参数等价于传空串。
 */

#ifndef BATTLE_PROXY_H
#define BATTLE_PROXY_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 版本串，例如 "2.3.7-r39"。静态内存。 */
const char *battle_proxy_version(void);

/*
 * 启动接收器。
 *   config_json: 配置 JSON（见 INTERFACES.md §2）；可传 NULL / "" 使用默认配置。
 * 返回: status JSON（见 INTERFACES.md §3）。phase 为 "running" 或 "failed"。
 * 线程: 内部最多等待约 5 秒；请在后台线程调用（Swift 侧已在 Task.detached 中）。
 */
char *battle_proxy_start(const char *config_json);

/* 停止接收器。返回最终的 status JSON。 */
char *battle_proxy_stop(void);

/* 查询当前状态。返回 status JSON。 */
char *battle_proxy_status(void);

/*
 * 本机管理调用（不经 HTTP）。
 *   token: 配置里的 admin_token；不匹配则返回 {"ok":false,"reason":"invalid admin token"}
 *   path : 与 HTTP 路由同名，支持
 *          "diag" | "loot" | "session/reset" | "announcement" |
 *          "capture/start" | "capture/stop" | "capture/download" |
 *          "shutdown" | "selftest"
 *   body : JSON 字符串或 NULL；例如 loot 用 {"enabled":true}
 * 返回: JSON 字符串。
 */
char *battle_proxy_admin(const char *token, const char *path, const char *body);

/* 离线自检（不需要网络）。返回 JSON，ok=true 表示解码档位与配置仍然自洽。 */
char *battle_proxy_selftest(void);

/* 释放上面几个函数返回的字符串。传 NULL 是安全的。 */
void battle_proxy_free_string(char *p);

#ifdef __cplusplus
}
#endif

#endif /* BATTLE_PROXY_H */
