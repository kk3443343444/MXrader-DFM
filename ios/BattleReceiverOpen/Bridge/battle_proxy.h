/*
 * battle_proxy.h - C ABI exported by the Rust static library (libbattle_proxy.a).
 *
 * This header mirrors docs/INTERFACES.md 1 exactly. Do not add, rename or
 * reorder declarations: the Rust side (core/src/ios_bridge.rs) is generated
 * from the same contract, and any drift breaks the link step.
 *
 * Ownership rules (INTERFACES.md 1):
 *   - every returned char * is heap allocated, UTF-8, NUL terminated
 *   - the caller must release it with battle_proxy_free_string()
 *   - battle_proxy_version() returns a static string and must NOT be freed
 */

#ifndef BATTLE_PROXY_H
#define BATTLE_PROXY_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Build identifier, for example "2.3.7-r39". Static storage: never free it. */
const char *battle_proxy_version(void);

/* Start the receiver with config_json (INTERFACES.md 2). Returns status JSON. */
char *battle_proxy_start(const char *config_json);

/* Stop the receiver. Returns status JSON. */
char *battle_proxy_stop(void);

/* Snapshot of the current status (INTERFACES.md 3), without the web token churn. */
char *battle_proxy_status(void);

/* Local-only admin channel: path is one of the /api/admin/* routes, body is JSON or NULL. */
char *battle_proxy_admin(const char *token, const char *path, const char *body);

/* Release any char * returned by start/stop/status/admin. NULL is a no-op. */
void battle_proxy_free_string(char *p);

#ifdef __cplusplus
}
#endif

#endif /* BATTLE_PROXY_H */
