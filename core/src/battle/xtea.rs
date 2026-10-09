//! UDP 解密层：**XTEA（64 轮）+ 8 × 16 B 密钥 bank 按 8 字节块轮换**。
//!
//! 这是参考实现 `MyRader6Pro.ipa`（`Payload/MyRaderPro.app/DeltaRadarIOS`，arm64）
//! 解 UDP 数据报时**内联**在解码器里的那套算法。参数逐条抄自
//! `reference/myraderpro/xtea_udp_evidence.txt` 与 `docs/REFERENCE_DECRYPT_PATH.md` §1；
//! 每条注释都带 **vm 地址**（`__TEXT` 起始 vm `0x100004000`，文件偏移 = vm −
//! `0x100000000`），任何人拿着 `dist/ref2/dump.py <vm>:<n>` 都能复核。
//!
//! ## 算法事实（逐条给出处）
//!
//! | 事实 | 出处（vm 地址） |
//! |---|---|
//! | delta = `-0x9E3779B9`（`w19 = 0x61C88647`，`add w16,w16,w19` 即 `sum -= delta`） | `0x100067b84`/`b88`、`0x100067c4c` |
//! | 解密 `sum` 初值 `0xC6EF3720` = 32 × delta | `0x100067c28`/`c2c` |
//! | 第二个计数器 `0xC6EF3720 − delta`（编译器把两半的 sum 拆成两个寄存器） | `0x100067c30`/`c34` |
//! | 32 次迭代 × 2 轮 = 64 轮 | `0x100067c38`（`mov w15,#0x20`） |
//! | 前半轮 key 索引 `(sum >> 11) & 3` | `0x100067c3c`（`ubfx w17,w16,#0xb,#2`） |
//! | 后半轮 key 索引 `sum & 3` | `0x100067c50`（`and w0,w14,#3`） |
//! | 轮函数 `((v<<4) ^ (v>>5)) + v`，再与 `sum + key[..]` 异或 | `0x100067c54..c60`、`0x100067c68..c80` |
//! | 方向是**解密**（`sub w13,w13,w17` / `sub w12,w12,w17`） | `0x100067c64`、`0x100067c84` |
//! | 每块的密钥 = `bank[block_index & 7]`，整块 16 B（`ldr q0`） | `0x100067c18`、`0x100067c20` |
//! | 块序号从 0 起、每块 +1 | `0x100067c0c`、`0x100067c98` |
//! | **只解密 `len & ~7` 字节**，尾部余数原样保留 | `0x100067c04`（`ands x8,x26,#~7`） |
//!
//! bank 的形态（8 × 128-bit = 128 B）来自调用点整块搬运：`0x10008b788..0x10008b7c0`
//! 把 `state+0x18, +0x28, …, +0x88` 共 8 个 16 字节槽拷进栈上缓冲区，再作为第三个参数
//! 交给解码器；类型名 `season_xtea::SeasonXteaKeyBank`（`rust_name_blob.txt`）与
//! `Vec<SeasonXteaKeyBank>` 的 `grow_one` 吻合。
//!
//! ## 字节序：**小端 u32**（明确选择 + 理由）
//!
//! 样本读块用 `ldp w12,w13,[x11]`（`0x100067c1c`）、读密钥字用 `ldr w17,[x17]`
//! （`0x100067c44`/`c78`）、回写用 `stp w12,w13,[x11],#8`（`0x100067c94`）。arm64 的
//! `ldr/ldp w` 是**小端**加载；密钥槽只被 `ldr q0`/`str q0`（`0x100067c20`/`c24`）整块
//! 16 B 搬运、没有任何字节翻转或 `rev` 指令。因此：
//!
//! * 块：`v0 = u32::from_le_bytes(block[0..4])`、`v1 = u32::from_le_bytes(block[4..8])`；
//! * 密钥：`key[i] = u32::from_le_bytes(key[4i..4i+4])`。
//!
//! 大端组词在数学上是同一个密码（只是把密钥每 4 字节反转），但**与样本不兼容**。
//! 两条测试把这层不确定性钉死了：
//! `matches_little_endian_known_answer_vector`（钉住本实现 = 小端），
//! `decrypts_canonical_xtea_vector`（用公开向量证明轮函数/轮数/索引规则是**标准 XTEA**）。
//! 万一将来实抓流量证明样本其实是大端，只需把 `key_words` 与 `decrypt_block`/
//! `encrypt_block` 里的 `from_le_bytes`/`to_le_bytes` 换掉，两条 KAT 会立刻变红，
//! 不会静默错下去。
//!
//! ## 与 `transport_crypto` 的关系
//!
//! 本模块只负责"给我 8 把钥匙，我把 8 的倍数那部分解密"。**密码学上无法自证解密成功**
//! （XTEA 没有 MAC，任何密钥都会产出"看起来随机"的 8 字节块），所以判定成功只能靠
//! 上层结构校验：`udpxin::PacketFramer::validate_datagram`（样本里的
//! `server_decode_gate`）。`transport_crypto::decode_packet` 就是这么用的。

/// TEA 家族 magic delta。样本用它的补码形式：`w19 = 0x61C88647 = -0x9E3779B9`，
/// 循环里是 `add w16, w16, w19` —— 即 `sum -= XTEA_DELTA`（`0x100067b84`/`b88`）。
pub const XTEA_DELTA: u32 = 0x9E37_79B9;

/// 解密起始 `sum` = `32 × delta = 0xC6EF3720`（`0x100067c28`/`c2c`）。
pub const XTEA_DECRYPT_SUM_INIT: u32 = XTEA_DELTA.wrapping_mul(XTEA_ROUNDS as u32);

/// 迭代次数：样本 `mov w15, #0x20`（`0x100067c38`），每次迭代 2 轮 ⇒ 64 轮。
pub const XTEA_ROUNDS: usize = 32;

/// 一个块 8 字节。
pub const XTEA_BLOCK_LEN: usize = 8;

/// 单把密钥 16 字节（128-bit）。
pub const XTEA_KEY_LEN: usize = 16;

/// bank 里有多少把钥匙（样本拷贝 8 个槽：`0x10008b788..0x10008b7c0`）。
pub const XTEA_BANK_SLOTS: usize = 8;

/// bank 的总字节数（128 B）。
pub const XTEA_BANK_LEN: usize = XTEA_BANK_SLOTS * XTEA_KEY_LEN;

/// 把 16 字节密钥读成 4 个**小端** u32（样本 `ldr w17,[x17]`，`0x100067c44`）。
#[inline]
fn key_words(key: &[u8; XTEA_KEY_LEN]) -> [u32; 4] {
    let mut w = [0u32; 4];
    for (i, word) in w.iter_mut().enumerate() {
        *word = u32::from_le_bytes([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]]);
    }
    w
}

/// XTEA 轮函数的混合步 `((v << 4) ^ (v >> 5)) + v`（`0x100067c54..c5c`）。
#[inline]
fn mix(v: u32) -> u32 {
    ((v << 4) ^ (v >> 5)).wrapping_add(v)
}

/// 原地解密**一个** 8 字节块。
///
/// 与样本 `0x100067c3c..0x100067c90` 的循环逐条对应：
///
/// ```text
/// for _ in 0..32 {
///     v1 -= (((v0 << 4) ^ (v0 >> 5)) + v0) ^ (sum + key[(sum >> 11) & 3]);
///     sum -= delta;
///     v0 -= (((v1 << 4) ^ (v1 >> 5)) + v1) ^ (sum + key[sum & 3]);
/// }
/// ```
///
/// 注意两半的索引**不一样**（`(sum>>11)&3` vs `sum&3`）——这正是 XTEA 区别于 TEA 的
/// 特征，样本两条指令分别落在 `0x100067c3c` 与 `0x100067c50`。
pub fn decrypt_block(block: &mut [u8; XTEA_BLOCK_LEN], key: &[u8; XTEA_KEY_LEN]) {
    let kw = key_words(key);
    let mut v0 = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    let mut sum = XTEA_DECRYPT_SUM_INIT;
    for _ in 0..XTEA_ROUNDS {
        v1 = v1.wrapping_sub(mix(v0) ^ sum.wrapping_add(kw[((sum >> 11) & 3) as usize]));
        sum = sum.wrapping_sub(XTEA_DELTA);
        v0 = v0.wrapping_sub(mix(v1) ^ sum.wrapping_add(kw[(sum & 3) as usize]));
    }
    block[0..4].copy_from_slice(&v0.to_le_bytes());
    block[4..8].copy_from_slice(&v1.to_le_bytes());
}

/// 原地加密**一个** 8 字节块 —— `decrypt_block` 的精确逆（标准 XTEA encipher）。
///
/// 参考实现里**不存在**这个方向（它只解密），这里提供它只有两个用途：
/// 1. 往返测试（`decrypt(encrypt(x)) == x`）与"尾部不变"测试的夹具；
/// 2. 将来做本地端到端验证实验时造密文（报告 §9 第 3 条）。
///
/// 不要把它当成"游戏上行加密"的实现：上行方向的算法/密钥在样本里**未确认**。
pub fn encrypt_block(block: &mut [u8; XTEA_BLOCK_LEN], key: &[u8; XTEA_KEY_LEN]) {
    let kw = key_words(key);
    let mut v0 = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    let mut sum = 0u32;
    for _ in 0..XTEA_ROUNDS {
        v0 = v0.wrapping_add(mix(v1) ^ sum.wrapping_add(kw[(sum & 3) as usize]));
        sum = sum.wrapping_add(XTEA_DELTA);
        v1 = v1.wrapping_add(mix(v0) ^ sum.wrapping_add(kw[((sum >> 11) & 3) as usize]));
    }
    block[0..4].copy_from_slice(&v0.to_le_bytes());
    block[4..8].copy_from_slice(&v1.to_le_bytes());
}

/// 8 把 XTEA 密钥的 bank，按 **8 字节块序号** 轮换：`key = bank[block_index & 7]`。
///
/// 密钥**不在样本二进制里**（报告 §4 用 4 种方法确认：0 命中、无 128 B 高熵常量块），
/// 它来自运行期状态对象 `state+0x18..+0x88`。所以这个类型的设计目标是
/// **"让上游能把 8 把钥匙塞进来"**，而不是"内置一份钥匙"：
///
/// ```text
/// let mut bank = XteaKeyBank::default();
/// bank.set_key(0, k0); ... bank.set_key(7, k7);   // 或 XteaKeyBank::from_keys([k0..k7])
/// keys.install_xtea_bank(bank);                    // transport_crypto::TransportKeys
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XteaKeyBank {
    keys: [[u8; XTEA_KEY_LEN]; XTEA_BANK_SLOTS],
}

impl Default for XteaKeyBank {
    fn default() -> Self {
        Self { keys: [[0u8; XTEA_KEY_LEN]; XTEA_BANK_SLOTS] }
    }
}

impl XteaKeyBank {
    /// 直接给出 8 把钥匙（生成顺序 = 参考实现拷贝顺序 `state+0x18..+0x88`）。
    pub fn from_keys(keys: [[u8; XTEA_KEY_LEN]; XTEA_BANK_SLOTS]) -> Self {
        Self { keys }
    }

    /// 从上游拿到的一整块 128 B（例如 TCP MitM 里 `x11+0x43` 那种缓冲区）构造。
    /// 第 `i` 把钥匙 = `bank[i*16 .. i*16+16]`，**不做任何字节重排**（样本是 `ldr q0`）。
    pub fn from_bytes(bank: &[u8; XTEA_BANK_LEN]) -> Self {
        let mut keys = [[0u8; XTEA_KEY_LEN]; XTEA_BANK_SLOTS];
        for (i, k) in keys.iter_mut().enumerate() {
            k.copy_from_slice(&bank[i * XTEA_KEY_LEN..(i + 1) * XTEA_KEY_LEN]);
        }
        Self { keys }
    }

    /// 写第 `idx` 把钥匙。索引按 `idx & 7` 取模 —— 与解密侧的
    /// `and x14, x9, #7`（`0x100067c18`）同一套轮换语义，这样调用方即使传了
    /// `block_index` 也不会越界。
    pub fn set_key(&mut self, idx: usize, key: [u8; XTEA_KEY_LEN]) {
        self.keys[idx & (XTEA_BANK_SLOTS - 1)] = key;
    }

    /// 全部 8 把钥匙（只读）。索引 0..7 与样本 `state+0x18,+0x28,…,+0x88` 一一对应。
    pub fn keys(&self) -> &[[u8; XTEA_KEY_LEN]; XTEA_BANK_SLOTS] {
        &self.keys
    }

    /// 第 `block_index` 个 8 字节块用哪把钥匙：`bank[block_index & 7]`。
    pub fn key_for_block(&self, block_index: usize) -> &[u8; XTEA_KEY_LEN] {
        &self.keys[block_index & (XTEA_BANK_SLOTS - 1)]
    }

    /// 是否还一把钥匙都没装（全 0）。DB 里默认构造出来的 bank 就是这个状态，
    /// 用来避免"把占位 bank 当成真钥匙"。
    pub fn is_all_zero(&self) -> bool {
        self.keys.iter().all(|k| k.iter().all(|b| *b == 0))
    }

    /// bank 指纹（FNV-1a 64）：诊断日志里用它区分"换了一把钥匙"和"协议变体不对"，
    /// 对应参考实现那套 `lastKeyFingerprint` 指标（报告 §8/R4）。不含密钥材料本身的
    /// 明文，只有 16 个 hex 字符，日志里可以放心打。
    pub fn fingerprint(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for k in &self.keys {
            for b in k {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }

    /// 原地解密：**只处理 `len & ~7` 字节**，尾部不足 8 字节的余数**原样保留**。
    ///
    /// 这条"不碰尾巴"的规则直接来自样本：`ands x8, x26, #0x7ffffffffffffff8`
    /// （`0x100067c04`）算出循环计数，`subs x8, x8, #8` 逐块递减（`0x100067c9c`），
    /// 循环体外没有任何对余数的处理。块序号从 0 开始（`0x100067c0c`），每块 +1
    /// （`0x100067c98`），所以密钥轮换也是从 `bank[0]` 起算的**包内**序号。
    pub fn decrypt_in_place(&self, buf: &mut [u8]) {
        let full = buf.len() & !(XTEA_BLOCK_LEN - 1);
        for (i, chunk) in buf[..full].chunks_exact_mut(XTEA_BLOCK_LEN).enumerate() {
            let mut block = [0u8; XTEA_BLOCK_LEN];
            block.copy_from_slice(chunk);
            decrypt_block(&mut block, self.key_for_block(i));
            chunk.copy_from_slice(&block);
        }
    }

    /// `decrypt_in_place` 的逆（同样只动 `len & ~7` 字节）。测试夹具/造密文用。
    pub fn encrypt_in_place(&self, buf: &mut [u8]) {
        let full = buf.len() & !(XTEA_BLOCK_LEN - 1);
        for (i, chunk) in buf[..full].chunks_exact_mut(XTEA_BLOCK_LEN).enumerate() {
            let mut block = [0u8; XTEA_BLOCK_LEN];
            block.copy_from_slice(chunk);
            encrypt_block(&mut block, self.key_for_block(i));
            chunk.copy_from_slice(&block);
        }
    }

    /// 复制式解密（不改调用方的缓冲区）。尾部余数照抄。
    pub fn decrypt(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = buf.to_vec();
        self.decrypt_in_place(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可辨识 bank：`bank[i][j] = i*16 + j`。任何一次密钥轮换错位都会改变结果。
    fn demo_bank() -> XteaKeyBank {
        let mut keys = [[0u8; XTEA_KEY_LEN]; XTEA_BANK_SLOTS];
        for (i, k) in keys.iter_mut().enumerate() {
            for (j, b) in k.iter_mut().enumerate() {
                *b = (i * XTEA_KEY_LEN + j) as u8;
            }
        }
        XteaKeyBank::from_keys(keys)
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        hex::decode(s).expect("hex 向量必须是合法 hex")
    }

    /// 公开的 TEA/XTEA 测试向量（`key = 000102…0f`、明文 `"ABCDEFGH"`、密文
    /// `497DF3D072612CB5`，见 klockstone 的 TEA/XTEA 向量表）。该表把 hex 串按
    /// **大端**组词，所以这里对每个 4 字节组做一次重排，喂给本模块的小端实现：
    /// 这样测试证明的是"轮函数/轮数/delta/两条 key 索引规则 = 标准 XTEA"，
    /// 而字节序由下一条 KAT 单独钉住。
    #[test]
    fn decrypts_canonical_xtea_vector() {
        let key: [u8; 16] = hex_bytes("03020100070605040b0a09080f0e0d0c").try_into().unwrap();
        let mut block: [u8; 8] = hex_bytes("d0f37d49b52c6172").try_into().unwrap();
        decrypt_block(&mut block, &key);
        assert_eq!(u32::from_le_bytes(block[0..4].try_into().unwrap()), 0x4142_4344, "v0 应为 'ABCD'");
        assert_eq!(u32::from_le_bytes(block[4..8].try_into().unwrap()), 0x4546_4748, "v1 应为 'EFGH'");
        // 同一组字节走加密方向必须回到公开密文（证明 encrypt/decrypt 互为逆）。
        encrypt_block(&mut block, &key);
        assert_eq!(block, hex_bytes("d0f37d49b52c6172")[..]);
    }

    /// 小端 KAT（由一个独立的 Python 参考脚本按"小端组词"生成）：
    /// key = `00 01 .. 0f`、块 = `01 02 .. 08` ⇒ 密文 `7675bdf438e2890e`。
    /// 这条把"块与密钥都按小端 u32 组词"钉死在测试里（样本 `ldp w` / `ldr w`）。
    #[test]
    fn matches_little_endian_known_answer_vector() {
        let key: [u8; 16] = hex_bytes("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut block: [u8; 8] = hex_bytes("0102030405060708").try_into().unwrap();
        encrypt_block(&mut block, &key);
        assert_eq!(block, hex_bytes("7675bdf438e2890e")[..], "小端 KAT 必须逐字节吻合");
        decrypt_block(&mut block, &key);
        assert_eq!(block, hex_bytes("0102030405060708")[..]);
    }

    /// 往返：8 的倍数、带余数、0 字节、单块都要成立；并且"不足 8 字节"的余数
    /// 一个字节都不许动。
    #[test]
    fn round_trip_covers_all_length_classes() {
        let bank = demo_bank();
        for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 63, 64, 65, 128, 131] {
            let orig: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let mut buf = orig.clone();
            bank.encrypt_in_place(&mut buf);
            let full = len & !(XTEA_BLOCK_LEN - 1);
            if full == 0 {
                assert_eq!(buf, orig, "len={len}: 不足 8 字节时整段原样（vm 0x100067c04）");
            } else {
                assert_ne!(buf[..full], orig[..full], "len={len}: 块区必须被变换");
                assert_eq!(buf[full..], orig[full..], "len={len}: 尾部余数必须原样");
            }
            bank.decrypt_in_place(&mut buf);
            assert_eq!(buf, orig, "len={len}: decrypt(encrypt(x)) 必须回到原文");
            // 0 字节：显式确认不 panic、不产出。
            assert_eq!(XteaKeyBank::default().decrypt(&orig[..0]).len(), 0);
        }
    }

    /// 单块（8 字节）也要走完整 64 轮，且 `decrypt_block` 与 `decrypt_in_place`
    /// 对第一块的结果必须一致 —— 证明块序号从 0 起算，用的是 `bank[0]`。
    #[test]
    fn single_block_uses_bank_zero() {
        let bank = demo_bank();
        let mut direct: [u8; 8] = hex_bytes("0102030405060708").try_into().unwrap();
        let mut via_slice = direct.to_vec();
        decrypt_block(&mut direct, bank.key_for_block(0));
        bank.decrypt_in_place(&mut via_slice);
        assert_eq!(direct[..], via_slice[..], "整段解密的第一块必须与单块解密一致");
        // 换成 bank[1] 必须得到不同结果（否则"轮换"就是假的）。
        let mut wrong: [u8; 8] = hex_bytes("0102030405060708").try_into().unwrap();
        decrypt_block(&mut wrong, bank.key_for_block(1));
        assert_ne!(direct, wrong);
    }

    /// 尾部不足 8 字节必须原样保留（显式一条，含"必须解密过的头"这一半）。
    #[test]
    fn tail_shorter_than_one_block_is_untouched() {
        let bank = demo_bank();
        let mut buf: Vec<u8> = (0..13u8).collect();
        let head = buf[..8].to_vec();
        let tail = buf[8..].to_vec();
        bank.decrypt_in_place(&mut buf);
        assert_eq!(buf[8..], tail[..], "尾部 5 字节必须原样（vm 0x100067c04）");
        assert_ne!(buf[..8], head[..], "块区必须被解密");
        // 反向：加密侧的尾部同样不许动，且整段可逆。
        let mut buf2: Vec<u8> = (0..13u8).collect();
        let original = buf2.clone();
        bank.encrypt_in_place(&mut buf2);
        assert_eq!(buf2[8..], original[8..], "加密侧尾部同样原样");
        assert_ne!(buf2[..8], original[..8], "加密侧块区必须被变换");
        bank.decrypt_in_place(&mut buf2);
        assert_eq!(buf2, original);
    }

    /// bank 轮换：8 个块必须**依次**取 `bank[0..7]`。
    ///
    /// 用的是外部脚本（独立 Python 实现）按 `bank[i][j]=i*16+j` 生成的 64 B 密文 ——
    /// 它只有在"第 b 块 = 第 b 把钥匙"时才解得回 `00..3f`。任何错位（例如全部用
    /// `bank[0]`、或序号从 1 起算）都会立刻失败。
    #[test]
    fn bank_rotates_eight_keys_by_block_index() {
        let bank = demo_bank();
        let cipher = hex_bytes(concat!(
            "256004e1f55bc0c72a15b6947d4e96568f6c5790accadd57337fcde6b2d4624a",
            "312f4537fbd6b83e32584e75fbd9cf0cf78dae397acf45e2ed6f4fc0c369bc62",
        ));
        assert_eq!(cipher.len(), 64);
        let mut buf = cipher.clone();
        bank.decrypt_in_place(&mut buf);
        let expect: Vec<u8> = (0..64u8).collect();
        assert_eq!(buf, expect, "8 个块必须依次用 bank[0..7] 解密");

        // 反面：8 个槽都塞 bank[0] 的退化 bank 解不出同一份密文。
        let degenerate = XteaKeyBank::from_keys([*bank.key_for_block(0); XTEA_BANK_SLOTS]);
        assert_ne!(degenerate.decrypt(&cipher), expect);

        // 正面：逐块用 `key_for_block` 自己解，结果必须与整体解密一致。
        let mut manual = cipher.clone();
        for (b, chunk) in manual.chunks_exact_mut(XTEA_BLOCK_LEN).enumerate() {
            let mut block: [u8; XTEA_BLOCK_LEN] = chunk.try_into().unwrap();
            decrypt_block(&mut block, bank.key_for_block(b));
            chunk.copy_from_slice(&block);
        }
        assert_eq!(manual, expect);
    }

    /// 轮换在第 9 块回到 `bank[0]`；`set_key` 用 `idx & 7` 取模，绝不越界。
    #[test]
    fn bank_rotation_wraps_and_set_key_masks_index() {
        let bank = demo_bank();
        assert_eq!(bank.key_for_block(0), bank.key_for_block(8));
        assert_eq!(bank.key_for_block(7), bank.key_for_block(15));
        assert_ne!(bank.key_for_block(0), bank.key_for_block(1));

        let mut b = XteaKeyBank::default();
        assert!(b.is_all_zero());
        for i in 0..XTEA_BANK_SLOTS {
            b.set_key(i, *bank.key_for_block(i));
        }
        assert_eq!(b, bank);
        b.set_key(9, [0xAB; XTEA_KEY_LEN]); // 9 & 7 == 1
        assert_eq!(b.keys()[1], [0xAB; XTEA_KEY_LEN]);
        assert_eq!(b.keys()[0], *bank.key_for_block(0));

        // 128 B 整块构造 / 指纹：可辨识 + 稳定。
        let mut flat = [0u8; XTEA_BANK_LEN];
        for (i, k) in bank.keys().iter().enumerate() {
            flat[i * XTEA_KEY_LEN..(i + 1) * XTEA_KEY_LEN].copy_from_slice(k);
        }
        assert_eq!(XteaKeyBank::from_bytes(&flat), bank);
        assert_eq!(bank.fingerprint(), 0x356c_6cc8_1375_14a5);
        assert_ne!(XteaKeyBank::default().fingerprint(), bank.fingerprint());
        assert_eq!(bank.fingerprint(), bank.fingerprint(), "指纹必须稳定");
    }

    /// 常量本身也要对（防止有人"顺手改参数"）：delta、sum 初值、轮数、bank 尺寸。
    #[test]
    fn algorithm_constants_match_the_disassembly() {
        assert_eq!(XTEA_DELTA, 0x9E37_79B9, "vm 0x100067b84/b88 -> w19 = 0x61C88647");
        assert_eq!(XTEA_DECRYPT_SUM_INIT, 0xC6EF_3720, "vm 0x100067c28/c2c");
        assert_eq!(XTEA_DECRYPT_SUM_INIT.wrapping_sub(XTEA_DELTA), 0x28B7_BD67, "vm 0x100067c30/c34");
        assert_eq!(XTEA_ROUNDS, 32, "vm 0x100067c38 -> mov w15,#0x20");
        assert_eq!(XTEA_BANK_LEN, 128, "vm 0x10008b788..b7c0 -> 8 x 16 B");
        assert_eq!(XTEA_BLOCK_LEN, 8, "vm 0x100067c94 -> stp w12,w13,[x11],#8");
    }
}
