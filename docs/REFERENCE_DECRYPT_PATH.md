# 参考实现解密链路逆向：`MyRader6Pro.ipa` → UDP 到底怎么解

> 样本：`MyRader6Pro.ipa`（1,418,000 B，SHA-256 `2AF3F6D4CD03B81F8B2A2E9F371B85785D3BC3050C4EEDAB35BD153C7D08F9F1`）
> 主二进制：`Payload/MyRaderPro.app/DeltaRadarIOS`（2,624,688 B，arm64，SHA-256 `B0DA32108309331AADF6953C889DA35CBDDE270D2D2A8F217E5A1D6AE13A9E18`）
> 只读分析：原始 `.ipa` 未修改；解包副本在 `dist/mp/x/`（首次构建产物，可删）
> 上一轮报告：`docs/REFERENCE_MYRADERPRO.md`（本文件只补充/修正它，不重复其内容）
> 新增资产：`reference/myraderpro/rust_name_blob.txt`、`xtea_udp_evidence.txt`、`tgcp_mode3_evidence.txt`
> 本文所有偏移约定：**代码用 vm 地址**（`0x100xxxxxx`，`__TEXT` 起始 vm `0x100004000` = 文件 `0x4000`，故文件偏移 = vm − `0x100000000`）；**字符串/常量用文件偏移**。
> 事实与推断严格分开：§1–§5 是已验证事实，§6 是推断与置信度，§7 是未确认清单。

---

## 当前阶段 / Current phase

深度逆向（第 5 阶段）。目标：判定 UDP 是否加密、算法、密钥来源、能否复刻。**已拿到可复现的直接证据链**，结论足够支撑产品路线决策。

---

## 0. 一句话结论

1. **UDP 数据报是加密的，算法是 XTEA（64 轮、delta `0x9E3779B9`、128-bit 密钥），而且参考实现是在本机自己解密的**（不是只把密文丢给云）。
2. **密钥不是一个，而是"8 个 XTEA 密钥的 bank（8×16 B = 128 B）"，按 8 字节块序号轮换：`key = bank[block_index & 7]`。**
3. 这个 bank 在二进制里**没有任何常量副本**（上一轮"0 命中"的结论我用 4 种不同方法再次确认），它来自运行期状态对象；提取链路指向**对游戏自身 TCP（TGCP）的主动 MitM**，**不是**从它的云下发，**也**不是内嵌赛季表。
4. 对我们最省事的路径：**XTEA + 8 键 bank 是完全可复刻的（小工作量）**；瓶颈在**"这 8 把钥匙从哪来"**——需要复刻 TGCP 的握手/密钥提取（大工作量，且仍有未确认字段）。

---

## 1. 已验证事实：UDP 用 XTEA 解密

### 1.1 位置

| 项 | vm | 文件偏移 |
|---|---|---|
| 数据报解码函数主入口（被 `bl` 引用） | `0x100067ac0` | `0x67ac0` |
| 同一函数第二入口（`sub sp,sp,#0x280` 处） | `0x100067b5c` | `0x67b5c` |
| 调用点（`bl 0x100067b5c`） | `0x10008b68c`、`0x10008b6a0`、`0x10008b748`、`0x10008b75c` | — |
| UDP_C 分帧错误串 | — | `0x1b986f` 起（见 §1.4） |

该函数的 callee 集合（`bl`）：`0x10008b920`（位读取器/`Bunch` 辅助）、`0x10015ab30`（共享基础设施，全库 34 处调用）、`0x100098eb4/8/c0/c4`（分配器）、`0x100198d34`/`0x10019a464`/`0x1001a3e5c`/`0x1001a4060`（panic/alloc 辅助）、`0x1001a56fc`（memcpy）。
**没有任何密码学调用**——XTEA 是**内联**在这个函数里的（见下）。

### 1.2 算法常量（反汇编原文）

```
0x100067b84  mov   w19, #0x8647            ; |
0x100067b88  movk  w19, #0x61c8, lsl #16   ; +-> w19 = 0x61C88647 = -0x9E3779B9  (TEA/XTEA delta)
0x100067c28  mov   w16, #0x3720            ; |
0x100067c2c  movk  w16, #0xc6ef, lsl #16   ; +-> w16 = 0xC6EF3720 = 32 * 0x9E3779B9  (解密起始 sum)
0x100067c30  mov   w14, #0xbd67            ; |
0x100067c34  movk  w14, #0x28b7, lsl #16   ; +-> w14 = 0x28B7BD67 = 0xC6EF3720 - 0x9E3779B9
0x100067c38  mov   w15, #0x20              ; +-> 32 次迭代（每次 2 轮 = 64 轮，标准 XTEA）
```

`0x28B7BD67` 不是新常量：`0xC6EF3720 − 0x9E3779B9 = 0x28B7BD67`，即编译器把"两半各用一次 sum（相差一个 delta）"拆成两个计数器。

### 1.3 轮函数（XTEA 解密，逐条）

```
0x100067c18  and   x14, x9, #7              ; 块序号 & 7  -> bank 索引
0x100067c1c  ldp   w12, w13, [x11]          ; 取 8 字节块 = v0,v1（小端 u32）
0x100067c20  ldr   q0,  [x10, x14, lsl #4]  ; bank[block_index & 7]，16 字节 = 128-bit 密钥
0x100067c24  str   q0,  [x22]               ; 拷到栈上的 16 字节密钥槽
  循环（0x100067c3c..0x100067c90，32 次）:
0x100067c3c  ubfx  w17, w16, #0xb, #2       ; key index = (sum >> 11) & 3   ← XTEA 特征
0x100067c40  orr   x17, x22, x17, lsl #2
0x100067c44  ldr   w17, [x17]               ; key[(sum>>11)&3]
0x100067c48  add   w17, w16, w17            ; sum + key[...]
0x100067c4c  add   w16, w16, w19            ; sum += (-delta)   （等价 sum -= delta）
0x100067c54  lsl   w1,  w12, #4             ; ((v0 << 4)
0x100067c58  eor   w1,  w1,  w12, lsr #5    ;      ^ (v0 >> 5))
0x100067c5c  add   w1,  w1,  w12            ;      + v0
0x100067c60  eor   w17, w17, w1
0x100067c64  sub   w13, w13, w17            ; v1 -= ( (sum+key) ^ (((v0<<4)^(v0>>5))+v0) )
0x100067c68  lsl   w17, w13, #4
0x100067c6c  eor   w17, w17, w13, lsr #5
0x100067c70  add   w17, w17, w13
0x100067c7c  add   w0,  w14, w0             ; sum2 + key[sum2 & 3]
0x100067c80  eor   w17, w17, w0
0x100067c84  sub   w12, w12, w17            ; v0 -= ...
0x100067c88  add   w14, w14, w19            ; sum2 += (-delta)
0x100067c8c  subs  x15, x15, #1
0x100067c90  b.ne  #0x100067c3c
0x100067c94  stp   w12, w13, [x11], #8      ; 写回 8 字节块，指针 +8
0x100067c98  add   x9, x9, #1               ; block_index++
0x100067c9c  subs  x8, x8, #8               ; 计数 = (len & ~7)，逐块递减
0x100067ca0  b.ne  #0x100067c18
```

判定依据（逐条对应 XTEA 规范）：`lsl #4` + `lsr #5` + `add v` 的轮结构、`(sum>>11)&3` 的密钥索引、`sum` 初值 = 32·delta、`sub` 方向（解密）、32 次迭代。**不是 AES、不是 XXTEA、不是 XOR 流。**

**范围**：只解密 `len & ~7` 个字节（循环计数 `x8 = x26 & 0x7fff...f8`），**尾部不足 8 字节的部分保持原样**。

**解密前**：数据报被 `memcpy` 到新分配缓冲（`0x100067bf4`），**原地解密**；该路径**无条件解密**（没有开关分支）。上游是否只把"已绑定密钥的流"塞进这个队列 = 未确认（见 §7）。

### 1.4 解密后直接进 UDP_C 分帧

同函数内的分帧错误串（全部 `__TEXT,__const`，文件偏移）：

```
0x1b986f  UDP_C packet exceeds bunch limit
0x1b98d9  UDP_C notify framing bit is set
0x1b9926  UE packed uint exceeds 10 octets
0x1b98b1  UDP_C notify history exceeds packet / packet is shorter than its notify header
0x1b9a92  UDP_C packet has no complete bunch boundary / bunch channel index is outside the live range
0x1b9b20  TGCP mode-3 padding or trailer mismatch ...      ← TCP 侧（§2）
```
xref 到这些串的代码：`0x100068e28`、`0x100068f9c`、`0x10008bab4`（位读取器）→ 全在该解码函数体内（函数延伸到 `0x1000692xx`）。

### 1.5 密钥 bank 的形态（8×16 B）

解码器第三个参数（`x2`）解引用得到状态对象，再取 `+0x18` 作为 bank 基址（`0x100067bd8..0x100067c10`）。调用方把 **8 个 16 字节槽**（`+0x18,+0x28,…,+0x88`）整体拷进栈上缓冲区：

```
0x10008b788  ldur q0, [x25, #0x58] ; stur q0, [x21, #0xd8]
0x10008b790  ldur q0, [x25, #0x68]
0x10008b798  ldur q0, [x25, #0x78]
0x10008b7a0  ldur q0, [x25, #0x88]
0x10008b7ac  ldur q0, [x25, #0x18] ; stur q0, [x22]        ; key[0]
0x10008b7b0  ldur q1, [x25, #0x28] ; stp  q0,q1, [x22]      ; key[1]
0x10008b7b8  ldur q0, [x25, #0x38]
0x10008b7bc  ldur q1, [x25, #0x48] ; stp  q0,q1, [x22,#0x20]
```
→ **bank = 128 字节 = 8 × 128-bit**，与类型名 `season_xtea::SeasonXteaKeyBank` 吻合。

### 1.6 全库只有这一处 XTEA

- 用修正后的 MOVZ/MOVK 掩码（`bits 28:23 == 0b100101`）扫描全部 426,183 条指令，TEA 家族常量只出现在 4 个簇：`0x100067b84-0x100067c34`（本节）、`0x1000630d8`（是 `TypeId` 比较常量，巧合）、`0x10014c4dc`/`0x10019e368`/`0x10019e65c`（64 位常量装配，哈希/TypeId 用途）。
- 全库 `0x9E3779B9`、`0x61C88647`、`0xC6EF3720` 的**原始 4 字节常量**（LE/BE）命中数 **0**——即：常量只以 `movz/movk` 指令形式存在，不存在任何"密钥表/常量表"副本。
- 索引寻址的 32-bit 表加载（`ldr w, [xN, xM, lsl #2]`）命中 **0**，`and w, w, #3` 命中 **0**——除本处内联实现外**没有第二份 TEA 家族实现**。

> 这三条否定了"二进制里藏着一份 XTEA/季节密钥表"的假设，也解释了我们上一轮为什么"看不见 XTEA"：当时只做了**原始字节**搜索，而常量是指令立即数。

---

## 2. 已验证事实：TCP（TGCP）方向是"16 字节块密码 + 自定义 padding/trailer"

这一节是为了回答"密钥从哪来"。

| 项 | vm | 说明 |
|---|---|---|
| mode-3 解密包装函数入口 | `0x10008bbf8` | `tst x2,#0xf; b.ne → "TGCP mode-3 ciphertext is not block aligned"` → **16 字节块对齐** |
| 块原语 | `0x10015bd14` | 轮密钥 XOR（`[x20,#0x280]`/`[x20,#0x2a0]`）+ bitsliced 块变换 `0x10015b704` |
| 密钥建立 | `0x10015cac8` | 被包装函数以 `sret(sp+0x28)` 调用，写入 `0x280` 字节上下文 |
| mode-3 调用点 | `0x100088cc0`、`0x100088d1c`、`0x100088df8` | 全在 tgcp 模块；密钥指针 = `state + 0xd2` |
| 密钥安装 | `0x100088c60..0x100088c84` | 写 flag `state+0xd1=1`，再把 16 字节写到 `state+0xd2..0xe1`（来源 `x11+0x43`） |
| 方向二 | `state+0xc0`（flag）/ `+0xc1`（key） | 同一函数内成对使用 → 上行/下行各一把 16 字节密钥 |

观察到的块处理（`0x10008bc14..`）：逐 16 字节块调用 `0x10015bd14`，**每块结果再异或一个 16 字节常量**（文件 `0x1aa820`：`00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f`）；随后校验收尾：末字节 padlen `≤ 6`、`sub x8, x8, padlen`、`(0x10 或 0x20) - (len & 0xf) == padlen`、并读 `[x9-6]`（4 字节 trailer）→ 对应错误串 `TGCP mode-3 padding or trailer mismatch` / `padding is out of range` / `plaintext is too short`。

该常量池（文件 `0x1aa820..0x1aa8a0`）随后是 `0x63`、`0x4`、`0x0`、`0xffffffffffffffff` 与 **80 字节高熵常量**；`0x63` 是 AES 仿射常量、这组布局与 RustCrypto `aes` 的 bitslice（fixslice）常量块相符。配合依赖树里唯一被链接的对称分组密码 crate 是 `aes-0.8.4/src/soft/fixslice64.rs`（panic location 原文），**mode-3 的块密码高度指向 AES**（ECB/CBC 与 128/192/256 位未确认，见 §7）。

### 2.1 DH（密钥交换）事实

| 项 | 位置 | 证据 |
|---|---|---|
| 512-bit 素数（128 hex） | 文件 `0x1b94db` | `97981e0a…2fede3`，被 2 个函数各引用 2 次 |
| 密钥对生成 | vm `0x100083a44` | 引用素数 + `valid TGCP prime`(文件 `0x1aa8e0`)、`generated TGCP private exponent`(`0x1b955c`)、`TGCP DH private exponent is outside the accepted range`(`0x1b94a4`) |
| 共享密钥计算 | vm `0x100084190` | 引用素数 + `TGCP DH peer public value is outside the accepted range`、`TGCP DH shared secret is degenerate`(`0x1b944c`) |
| 大数运算 | crate `num-bigint-0.4.8` | 源路径串 |
| 主动替换语义 | 串 `TGCP replacement public value changes the header length` | 说明是 **MitM 改写公钥值**，不是被动旁路 |
| 帧日志 | vm `0x1000892b4`（tgcp 模块） | 格式串 `TGCP … opcode=0x… header=… payload=`（文件 `0x1b8d7b`/`0x1b8d7d`） |

---

## 3. 已验证事实：密钥上报（云）通道只看到"上行"

- `mirror key frame: `（文件 `0x213dc1`）xref → vm `0x10007a7bc`、`0x10007aa40`：这是 **日志**（`fmt::write`，callee `0x10018b7f8`），紧随其后把 24 字节（`Vec` 头）拷进结构 `state+0x98` → 即"已编码的 key frame 入队/发送"。
- 相关名字（`reference/myraderpro/rust_name_blob.txt`）：`MirrorQueue::record`、`MirrorUploader::pop_next_frame`、`QueuedMirrorFrame`、`MirrorBuffer`、`KeyMaterial`。
- **`drk1` 在本二进制里只有 3 处，全部是指标键名**（文件 `0x1b93a1`/`0x1b93af`/`0x1b93bb` = `drk1KeysQueued`/`drk1KeysSent`/`drk1Conflicts`）；**不存在 `drk1` wire magic**。所以上一轮把 `drk1` 说成"密钥帧魔数"是不成立的——它只是计数器名。
- 未见任何"从 WSS 下行解析密钥表"的代码路径（只有 `encode_frame`/`encode_key_frame`/`key_is_unsent` 这类待发语义 + `ingest requires wss`/`invalid ingest identity` 校验）。

---

## 4. 已验证事实：没有可复用的密钥/映射表（4 种方法交叉确认）

| 方法 | 结果 |
|---|---|
| 128/16 字节高熵常量块扫描（`__TEXT,__const`＋`__DATA_CONST`，H>4.2） | 命中项全部可归类：AES bitslice 常量、libm/compiler-rt 浮点表、rustls/x509 的 DER-OID 串、base64/字符分类表。**没有任何 128 字节（8×16）密钥 bank 副本** |
| ≥32 字符长 hex | 全库唯一 = TGCP DH prime（`0x1b94db`） |
| ≥120 字符 base64 块 | 全部是 rustls 错误枚举名连排 + DH prime，无二进制资产 |
| `channel_map` / `handles` / `SOL_DT` / `loot_ids` / `slot_map` / `include_bytes` / `maps.json` / `name_cn` / `yaw_offset` | **全部 0 命中**（重新解包后再次确认） |
| `season` / `Season` 字符串 | 各 **1** 命中，且都在 `rust_name_blob` 里（`season_xtea17SeasonXteaKeyBank…`），**没有赛季密钥表数据** |

**结论：拿不到任何可直接复用的密钥材料或知识表。**

---

## 5. 已验证事实：模块/类型清单（来自 file `0x259930–0x271408` 的 1594 条名字碎片）

`reference/myraderpro/rust_name_blob.txt` 里与本题相关的（v0 mangling 片段，原文截取）：

```
…delta_radar_edge10key_broker4FlowEE14reserve_rehash…HashMap…SocketAddr…key_broker4Flow…
…delta_radar_edge13dr_edge_starts_0…      / …13dr_edge_starts_0…        ← 对 Swift 的入口
…delta_radar_edge10key_brokerNtB2_13TgcpKeyBroker…
…delta_radar_edge17QueuedMirrorFrameE8grow_one…
…key_broker9CandidateE8grow_one…          ← Vec<Candidate>
…season_xtea17SeasonXteaKeyBankE8grow_one… ← Vec<SeasonXteaKeyBank>
…unreal_probe5BunchE8grow_one…             ← Vec<Bunch>
…delta_radar_edge4tgcpNtB4_11TgcpSession…
…delta_radar_edgeNtB4_11MirrorQueue6record…
…delta_radar_edgeNtB5_14MirrorUploader14pop_next_frame…
…delta_radar_edge9relay_udp0NtB4_8KeyGuardNtNtNtD…4Drop4drop…
…KeyMaterialE9drop_slow…/…MirrorBufferE9drop_slow…
…key_broker13TgcpKeyBrokerEE9drop_slow…
```

即：`key_broker{TgcpKeyBroker, Candidate, Flow(HashMap<SocketAddr,Flow>)}`、`season_xtea::SeasonXteaKeyBank`（在 `Vec` 里）、`unreal_probe::Bunch`、`relay_udp::KeyGuard`（`Drop` 时释放绑定）、`tgcp::TgcpSession`、`MirrorQueue/MirrorUploader/QueuedMirrorFrame/MirrorBuffer`、`KeyMaterial`、`dr_edge_start*`。

---

## 6. 推断与置信度 / Inference and confidence

| # | 推断 | 置信度 | 支持证据 | 反证/缺口 |
|---|---|---|---|---|
| I1 | **密钥来源 = (a) 从"游戏自身 TCP 流"（被主动 MitM 的 TGCP）提取，不是 (b) 云下发、也不只是 (c) 内嵌表** | **中-高** | ① 指标名与中文标签：`密钥提取/已上报`(`tgcpKeysExtracted`)`候选密钥/已验证绑定`(`tgcpKeyCandidates`/`tgcpVerifiedKeyBindings`)`密钥归属 UDP`(`lastKeyAssociation`/`lastKeyEndpoint`)`密钥验证峰值`(`tgcpKeyProbeMaxUs`/`lastKeyFingerprint`)；② 本机确实实现 TGCP MitM（512-bit DH + 公钥替换 + mode-3 分组密码）与 UDP XTEA 解密；③ 无内嵌密钥表（§4）；④ 未见云→端密钥下发路径（§3） | 密钥"从哪一个帧/字段提取"**未能定位**（tgcp 是异步状态机，2,737 处 `br/blr` 间接调用，静态难追）；不能 100% 排除云通过 WSS 下发过密钥集 |
| I2 | mode-3（TCP）块密码是 **AES**（RustCrypto `aes` 0.8.4 bitslice 实现） | 中-高 | 链接 crate 唯一对称分组密码 = `aes-0.8.4`（`soft/fixslice64.rs` panic location）；16 字节块对齐；bitslice 常量池（`0x63` 仿射常量 + 80 字节常量）；块函数含 231 次 `eor`/165 次 `and` 的 bitslice 特征 | 未确证密钥长度（128/192/256）与模式（ECB/CBC），未确证"每块异或 `00..0f`"是模式的一部分还是 fixslice 的字节序修正 |
| I3 | 8 键 bank 是**赛季级**（跨会话复用）密钥集 | 低-中 | 模块/类型名 `season_xtea::SeasonXteaKeyBank`；如果是一次性会话密钥，没必要做成"bank"并按块轮换 | 静态无法区分赛季级/会话级；DH 是**每连接**进行的，反而暗示会话级 |
| I4 | 该解码队列里**所有** UDP 数据报都被无条件 XTEA 解密 | 中 | `0x100067b5c` 体内解密无开关分支 | "谁入队"未确认：可能上游只把"已验证密钥绑定"的流入队（`relay_udp::KeyGuard` 正是按 `SocketAddr` 绑定/释放） |
| I5 | **"必须依赖它的云才能解 UDP" 不成立**；我们也**不必然**依赖它下发季节密钥 | 中-高 | 解密发生在客户端本机（§1）、密钥 bank 在本机状态对象里、无云→端密钥路径证据（§3） | 若 I1 的提取点最终落在"只有它的云能提供的那一步"上，则此推断失效——这正是下一步要反汇编确认的 |
| I6 | 我们复刻 XTEA + bank 后，能解出与参考实现**逐字节相同**的明文 | 中-高 | 算法参数（64 轮/`sum` 初值/key 索引/端序/只处理 `len&~7`）全部逐条确证 | 未经真实密文验证；缺 bank 时无法端到端自证 |

---

## 7. 未确认清单（不要当成事实用）

1. TGCP 帧的完整格式（header/opcode/payload 布局）、密钥提取发生在**哪个 opcode/哪个字段**。
2. mode-3 的 AES 密钥长度/模式；"每块异或 `00..0f`"的语义；padding+4 字节 trailer 的精确布局（只确证了校验公式）。
3. 8 把 XTEA 密钥的**具体来源**（DH 共享密钥 KDF？被解密的 TCP 控制帧字段？两者都有？），以及 bank 在状态对象 `+0x18..+0x98` 的写入点（静态未定位到写入者）。
4. bank 是否跨会话复用（赛季级 vs 会话级，I3）。
5. UDP 双向是否同一 bank、是否同一旋转规则（只看到一处解密实现，方向归属未确证）。
6. `state+0xc1`/`+0xd2` 两把 mode-3 密钥哪把是上行、哪把是下行。
7. 镜像 WSS（ingest）帧与 key frame 的二进制封装（仍无 JSON/schema/字段名）。

---

## 8. 风险/漏洞候选 / Risk candidates（针对**我方**实现）

| # | 风险 | 证据 | 影响 |
|---|---|---|---|
| R1 | **我们现在的 UDP"解密"是猜的**：`core/src/battle/transport_crypto.rs` 只有 `Plain / AES-ECB-XOR / LZ4 / AES+LZ4 / 滚动 XOR` 五种嗅探，**完全没有 XTEA**，靠 `decode_packet(validate=server_decode_gate)` 试错 | 该文件 §135-208；`TransportKind` 枚举 | 高：这是"解不出实体"的最直接原因 |
| R2 | **我们的默认 XOR 种子恰好是 XTEA 的 delta**（`XorStream::new` 里 `0x9E37_79B9`） | `transport_crypto.rs:105` | 中：说明当初把 delta 误当成 XOR 种子，容易给人"已经处理了加密"的错觉 |
| R3 | 缺 bank（8×16 B）与"按块轮换密钥"这一层结构 | §1.5 | 高：即使补上 XTEA，没有 bank 仍然是 0 明文 |
| R4 | 现架构失败时静默降级（`Decoded::Unknown` 保留原始字节），缺少参考实现那套诊断（`lastKeyFingerprint`/`tgcpKeyProbeMaxUs`/`候选密钥/已验证绑定`） | `transport_crypto.rs:206`；参考指标表 | 中：线上无法区分"密钥错"与"协议变体错" |

---

## 9. 建议下一步 / Suggested next steps（编号选项）

1. **出补丁方案（不改产品代码）**：我按 §Q5 写 `core/src/battle/xtea.rs` 的设计＋测试向量（8 键 bank、`block_index & 7`、`len & ~7`、64 轮），并给出接入 `transport_crypto::decode_packet` 的最小 diff 草案（只写设计文档，不动 `core/`）。
2. **继续反汇编 TGCP 密钥提取点**：重点 `0x100088874`（握手/帧校验）、`0x1000881f0`（帧后处理）、`0x100089408`（流缓冲）与 DH 调用周边，目标是确认"密钥从哪个帧字段来"——这条决定我们能否自给密钥。
3. **设计一次真实流量验证实验**：抓一份 A 机→游戏服的 UDP，验证①密文长度是否几乎都是 8 的倍数、②用 §1 的 XTEA 参数 + 一组合法 bank 是否能解出 `UDP_C` 合法 bunch（用我们现有 gate 判定），③是否明文包与密文包混流。
4. **写 r39 vs MyRaderPro 解密链路对比表**（FAES+AES-256-ECB+LZ4＋本地知识库 vs XTEA 8 键 bank＋云镜像），用于产品路线取舍。
5. **把 `rust_name_blob.txt` 做成可读的 demangle 清单**（函数级 inventory + 调用关系锚点），方便后续指定"反汇编某个函数"。
6. **到此为止**，直接进入实现/或先做 1+3。

---

## 10. 复现命令 / Reproduction

工具版本：`capstone 5.0.9`；Python `3.12.14`（`C:\Users\Administrator\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe`）。临时脚本全在 `dist/ref2/`（用完可删）：

| 脚本 | 作用 |
|---|---|
| `ml.py` | Mach-O 段/节解析、file↔vm 换算 |
| `xr.py` | capstone 全 `__text` 反汇编 + `ADRP(+ADD/LDR)` xref 表（426,183 条指令，7,186 个目标）→ `xrefs.pkl` |
| `cg.py` | `bl` 调用图（5,196 函数起点、2,763 个目标）→ `cg.pkl` |
| `entry.py` | 用"分支/指针可达地址"求函数真入口 → `entries.pkl` |
| `s16_movk.py` | **关键**：修正掩码的 MOVZ/MOVK 常量扫描（就是这一步找到 XTEA delta） |
| `s23_assets.py` | 生成 `reference/myraderpro/{rust_name_blob,xtea_udp_evidence,tgcp_mode3_evidence}.txt` |
| `dump.py` / `das.py` | 指定 vm 反汇编 + 常量区 hexdump |
| `q1.py` / `q2_metrics.py` / `q6_refs2.py` / `s17_bank.py` / `s19_bankwrite.py` / `s22_writer.py` | 字符串/metric/指针/bank 访问点查询 |
| `s10_tea.py` / `s11_tea2.py` / `s12_idiom.py` / `s13_dbg.py` / `s15_cryptodens.py` | 密码学指纹扫描（含**失败过一次**的实现，见下） |

```powershell
$py = "C:\Users\Administrator\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe"
# 0) 解包（只读分析，原始 ipa 不动）
Copy-Item "<wechat>\2026-10\MyRader6Pro.ipa" dist\mp\mp.zip -Force
Expand-Archive dist\mp\mp.zip -DestinationPath dist\mp\x -Force
Get-FileHash dist\mp\x\Payload\MyRaderPro.app\DeltaRadarIOS -Algorithm SHA256   # B0DA3210…A9E18
# 1) 全库反汇编 + xref + 调用图
& $py dist\ref2\xr.py; & $py dist\ref2\cg.py; & $py dist\ref2\entry.py 100067b5c
# 2) 常量扫描（XTEA delta 在这里现形）
& $py dist\ref2\s16_movk.py
# 3) 关键函数反汇编
& $py dist\ref2\dump.py 100067b5c:120      # XTEA 解密 + bank 选择
& $py dist\ref2\dump.py 10008b770:100      # 8 键 bank 整体拷贝到调用参数
& $py dist\ref2\dump.py 10008bbf8:70       # TGCP mode-3 包装（16 字节块 + pad/trailer）
& $py dist\ref2\dump.py d:1001aa800        # AES bitslice 常量池（0x63 + 80B 常量）
# 4) 生成参考资产
& $py dist\ref2\s23_assets.py
```

> **方法学纠正（写给下一个接手的人）**：上一轮与我这轮**第一遍**都得出"没有 XTEA"的结论，原因是三个实现 bug：
> ① MOVZ/MOVK 的 opcode 字段是 `bits 28:23 == 0b100101`，我误写成 `0b010101` → 立即数扫描 0 命中；
> ② TEA 轮里的 `lsr #5` 是 `eor` 的**操作数修饰符**（`eor w1, w1, w12, lsr #5`），按助记符 `lsr` 过滤会全部漏掉；
> ③ 只搜原始 4 字节常量，而 `0x61C88647` 只以 `movz w19,#0x8647; movk w19,#0x61c8,lsl#16` 形式存在。
> 教训：**在 ARM64 上找 crypto 常量必须走"指令立即数"而不是"字节模式"。**

---

## 11. 证据索引（速查）

```
XTEA 解密实现        vm 0x100067ac0 / 0x100067b5c     file 0x67ac0 / 0x67b5c
 delta                vm 0x100067b84,b88   (0x61C88647)
 sum 初值             vm 0x100067c28,c2c   (0xC6EF3720) ; 第二 sum 0x100067c30,c34
 轮数                  vm 0x100067c38      (#0x20 = 32 迭代)
 key 索引              vm 0x100067c3c      (ubfx w17,w16,#0xb,#2)
 轮函数                vm 0x100067c54..c84
 bank 选择             vm 0x100067c18,c20  (and x14,x9,#7 ; ldr q0,[x10,x14,lsl#4])
 调用点                vm 0x10008b68c, b6a0, b748, b75c
 bank 拷贝             vm 0x10008b788..b7c0 (8×16B from state+0x18..+0x88)
TGCP mode-3           vm 0x10008bbf8 (wrapper) / 0x10015bd14 (block) / 0x10015cac8 (key setup)
 mode-3 调用点         vm 0x100088cc0, 0x100088d1c, 0x100088df8 ; key = state+0xd2 (:c1)
 DH                    vm 0x100083198 / 0x100083a44 / 0x100084190
 DH prime (512-bit)    file 0x1b94db
 AES bitslice 常量池    file 0x1aa820..0x1aa8a0
 UDP_C 错误串          file 0x1b986f / 0x1b98d9 / 0x1b9926 / 0x1b9a92
 指标键表              file 0x1b90fd..0x1b9470（含密钥类指标）
 Rust 名字表           file 0x259930..0x271408（1594 条碎片）
 导入/bind             LC_DYLD_CHAINED_FIXUPS @ file 0x250000（912 imports, fmt=1）
 exports trie          file 0x259930（仅 2 个导出：__mh_execute_header, ___isPlatformVersionAtLeast）
```
