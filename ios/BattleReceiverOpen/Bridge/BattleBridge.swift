//
//  BattleBridge.swift
//  BattleReceiverOpen
//
//  Swift wrapper around the Rust static library C ABI (docs/INTERFACES.md 1).
//  The library is produced by ../scripts/build_rust.sh and linked in by the
//  "Build Rust static library" run-script phase declared in project.yml.
//
//  Design notes:
//    * Every char * returned by the C ABI is heap allocated and must be freed
//      with battle_proxy_free_string(). All calls go through `withResult` so a
//      thrown decoding error can never leak the buffer.
//    * start/stop/admin are blocking by contract, so they are invoked from a
//      detached task (never the main actor) by ReceiverModel.
//    * Only Apple frameworks are used (Foundation + Security for the admin
//      token; the C symbols come in through the bridging header).
//

import Foundation
import Security

// MARK: - Phase

/// Receiver lifecycle phase as reported by the Rust core.
/// The raw values are the ones spelled out in INTERFACES.md 3.
enum ReceiverPhase: String, Codable, CaseIterable, Sendable {
    case idle
    case preflight
    case starting
    case running
    case failed
    case stopping
    case unknown

    init(rawLenient: String?) {
        let normalized = (rawLenient ?? "").trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        self = ReceiverPhase(rawValue: normalized) ?? .unknown
    }

    var isRunning: Bool { self == .running }
    var isFailed: Bool { self == .failed }
    /// Phases where the splash screen is still the correct surface.
    var isTransient: Bool {
        switch self {
        case .idle, .preflight, .starting, .stopping, .unknown: return true
        case .running, .failed: return false
        }
    }
    /// Chinese label used by the UI chrome (never shown as a raw enum).
    var localizedLabel: String {
        switch self {
        case .idle: return "空闲"
        case .preflight: return "预检中"
        case .starting: return "启动中"
        case .running: return "运行中"
        case .failed: return "启动失败"
        case .stopping: return "停止中"
        case .unknown: return "未知"
        }
    }
}

// MARK: - Model objects (INTERFACES.md 3)

/// `endpoint` object of the status JSON.
struct ReceiverEndpoint: Codable, Equatable, Sendable {
    var interface: String
    var displayAddress: String?
    var radarDisplayAddress: String?
    var socksURL: String?
    var radarURL: String?

    enum CodingKeys: String, CodingKey {
        case interface
        case displayAddress = "display_address"
        case radarDisplayAddress = "radar_display_address"
        case socksURL = "socks_url"
        case radarURL = "radar_url"
    }

    init(
        interface: String = "0.0.0.0",
        displayAddress: String? = nil,
        radarDisplayAddress: String? = nil,
        socksURL: String? = nil,
        radarURL: String? = nil
    ) {
        self.interface = interface
        self.displayAddress = displayAddress
        self.radarDisplayAddress = radarDisplayAddress
        self.socksURL = socksURL
        self.radarURL = radarURL
    }

    static let empty = ReceiverEndpoint()
}

/// `runtime_status` object of the status JSON.
struct RuntimeStatus: Codable, Equatable, Sendable {
    var authorized: Bool
    var cardTail: String?
    var failedReason: String?

    enum CodingKeys: String, CodingKey {
        case authorized
        case cardTail = "card_tail"
        case failedReason = "failed_reason"
    }

    init(authorized: Bool = false, cardTail: String? = nil, failedReason: String? = nil) {
        self.authorized = authorized
        self.cardTail = cardTail
        self.failedReason = failedReason
    }

    static let unknown = RuntimeStatus()
}

/// 状态 JSON 里 `destinations` 的一条：目标 IP 的命中统计。
///
/// 这就是用户在诊断页里抄出来写"只代理游戏"分流规则的东西（Rust 侧 `crate::census`，
/// 按 IP 聚合 TCP CONNECT 目标 + UDP 转发的每个 dest，有界 128 条）。
/// 全部字段都用 `decodeIfPresent`：Rust 侧将来加字段或做字段改名时，这里只会退回默认值，
/// 不会让整个状态解码失败（`BattleBridge.status()` 解码失败会让健康轮询静默丢一帧）。
struct DestinationStat: Codable, Equatable, Sendable {
    var ip: String
    var packets: Int
    var lastMs: Int?

    enum CodingKeys: String, CodingKey {
        case ip
        case packets
        case lastMs = "last_ms"
    }

    init(ip: String = "—", packets: Int = 0, lastMs: Int? = nil) {
        self.ip = ip
        self.packets = packets
        self.lastMs = lastMs
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        ip = try container.decodeIfPresent(String.self, forKey: .ip) ?? "—"
        packets = try container.decodeIfPresent(Int.self, forKey: .packets) ?? 0
        lastMs = try container.decodeIfPresent(Int.self, forKey: .lastMs)
    }
}

/// Full status snapshot, decoded from the JSON produced by the C ABI.
/// Counters are top level in INTERFACES.md 3, hence the `decodeIfPresent` +
/// `decodeCounters` fallback (older builds nested them under `counters`).
struct ReceiverStatus: Codable, Equatable, Sendable {
    var phase: ReceiverPhase
    var mode: String?
    var version: String?
    var socksPort: UInt16?
    var webPort: UInt16?
    var primaryPort: UInt16?
    var dataDirectory: String?
    var webSessionToken: String?
    var endpoint: ReceiverEndpoint
    var runtimeStatus: RuntimeStatus

    var activeSessions: Int
    var totalSessions: Int
    var udpPacketsUp: Int
    var udpPacketsDown: Int
    var udpInvalidPackets: Int
    var tcpRelayFailures: Int
    var lootPayloadsSkipped: Int
    /// 目标地址统计（Rust `census`）：按包数降序，最多 32 条。
    var destinations: [DestinationStat]
    /// 见过的目标命中总量（含已被淘汰的目标）。
    var destinationsTotal: Int

    var lastHealthCheck: Date?
    var error: String?

    enum CodingKeys: String, CodingKey {
        case phase, mode, version, endpoint, error, destinations
        case socksPort = "socks_port"
        case webPort = "web_port"
        case primaryPort = "primary_port"
        case dataDirectory = "data_directory"
        case webSessionToken = "web_session_token"
        case runtimeStatus = "runtime_status"
        case activeSessions = "active_sessions"
        case totalSessions = "total_sessions"
        case udpPacketsUp = "udp_packets_up"
        case udpPacketsDown = "udp_packets_down"
        case udpInvalidPackets = "udp_invalid_packets"
        case tcpRelayFailures = "tcp_relay_failures"
        case lootPayloadsSkipped = "loot_payloads_skipped"
        case destinationsTotal = "destinations_total"
        case counters
        case lastHealthCheck = "last_health_check"
    }

    /// Nested counters block, used only as a fallback for older status payloads.
    private struct Counters: Codable {
        var activeSessions: Int?
        var totalSessions: Int?
        var udpPacketsUp: Int?
        var udpPacketsDown: Int?
        var udpInvalidPackets: Int?
        var tcpRelayFailures: Int?
        var lootPayloadsSkipped: Int?

        enum CodingKeys: String, CodingKey {
            case activeSessions = "active_sessions"
            case totalSessions = "total_sessions"
            case udpPacketsUp = "udp_packets_up"
            case udpPacketsDown = "udp_packets_down"
            case udpInvalidPackets = "udp_invalid_packets"
            case tcpRelayFailures = "tcp_relay_failures"
            case lootPayloadsSkipped = "loot_payloads_skipped"
        }
    }

    init(
        phase: ReceiverPhase = .idle,
        mode: String? = "ios_receiver",
        version: String? = nil,
        socksPort: UInt16? = nil,
        webPort: UInt16? = nil,
        primaryPort: UInt16? = nil,
        dataDirectory: String? = nil,
        webSessionToken: String? = nil,
        endpoint: ReceiverEndpoint = .empty,
        runtimeStatus: RuntimeStatus = .unknown,
        activeSessions: Int = 0,
        totalSessions: Int = 0,
        udpPacketsUp: Int = 0,
        udpPacketsDown: Int = 0,
        udpInvalidPackets: Int = 0,
        tcpRelayFailures: Int = 0,
        lootPayloadsSkipped: Int = 0,
        destinations: [DestinationStat] = [],
        destinationsTotal: Int = 0,
        lastHealthCheck: Date? = nil,
        error: String? = nil
    ) {
        self.phase = phase
        self.mode = mode
        self.version = version
        self.socksPort = socksPort
        self.webPort = webPort
        self.primaryPort = primaryPort
        self.dataDirectory = dataDirectory
        self.webSessionToken = webSessionToken
        self.endpoint = endpoint
        self.runtimeStatus = runtimeStatus
        self.activeSessions = activeSessions
        self.totalSessions = totalSessions
        self.udpPacketsUp = udpPacketsUp
        self.udpPacketsDown = udpPacketsDown
        self.udpInvalidPackets = udpInvalidPackets
        self.tcpRelayFailures = tcpRelayFailures
        self.lootPayloadsSkipped = lootPayloadsSkipped
        self.destinations = destinations
        self.destinationsTotal = destinationsTotal
        self.lastHealthCheck = lastHealthCheck
        self.error = error
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        phase = ReceiverPhase(rawLenient: try container.decodeIfPresent(String.self, forKey: .phase))
        mode = try container.decodeIfPresent(String.self, forKey: .mode)
        version = try container.decodeIfPresent(String.self, forKey: .version)
        socksPort = try container.decodeIfPresent(UInt16.self, forKey: .socksPort)
        webPort = try container.decodeIfPresent(UInt16.self, forKey: .webPort)
        primaryPort = try container.decodeIfPresent(UInt16.self, forKey: .primaryPort)
        dataDirectory = try container.decodeIfPresent(String.self, forKey: .dataDirectory)
        webSessionToken = try container.decodeIfPresent(String.self, forKey: .webSessionToken)
        endpoint = try container.decodeIfPresent(ReceiverEndpoint.self, forKey: .endpoint) ?? .empty
        runtimeStatus = try container.decodeIfPresent(RuntimeStatus.self, forKey: .runtimeStatus) ?? .unknown

        let legacy = try container.decodeIfPresent(Counters.self, forKey: .counters)
        activeSessions = try container.decodeIfPresent(Int.self, forKey: .activeSessions) ?? legacy?.activeSessions ?? 0
        totalSessions = try container.decodeIfPresent(Int.self, forKey: .totalSessions) ?? legacy?.totalSessions ?? 0
        udpPacketsUp = try container.decodeIfPresent(Int.self, forKey: .udpPacketsUp) ?? legacy?.udpPacketsUp ?? 0
        udpPacketsDown = try container.decodeIfPresent(Int.self, forKey: .udpPacketsDown) ?? legacy?.udpPacketsDown ?? 0
        udpInvalidPackets = try container.decodeIfPresent(Int.self, forKey: .udpInvalidPackets) ?? legacy?.udpInvalidPackets ?? 0
        tcpRelayFailures = try container.decodeIfPresent(Int.self, forKey: .tcpRelayFailures) ?? legacy?.tcpRelayFailures ?? 0
        lootPayloadsSkipped = try container.decodeIfPresent(Int.self, forKey: .lootPayloadsSkipped) ?? legacy?.lootPayloadsSkipped ?? 0
        // 目标地址统计：字段缺失（老版本核心）或元素字段缺失都不该让整份状态解码失败。
        destinations = try container.decodeIfPresent([DestinationStat].self, forKey: .destinations) ?? []
        destinationsTotal = try container.decodeIfPresent(Int.self, forKey: .destinationsTotal) ?? 0

        lastHealthCheck = BattleBridge.decodeTimestamp(try container.decodeIfPresent(String.self, forKey: .lastHealthCheck))
        error = try container.decodeIfPresent(String.self, forKey: .error)
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(phase.rawValue, forKey: .phase)
        try container.encodeIfPresent(mode, forKey: .mode)
        try container.encodeIfPresent(version, forKey: .version)
        try container.encodeIfPresent(socksPort, forKey: .socksPort)
        try container.encodeIfPresent(webPort, forKey: .webPort)
        try container.encodeIfPresent(primaryPort, forKey: .primaryPort)
        try container.encodeIfPresent(dataDirectory, forKey: .dataDirectory)
        try container.encodeIfPresent(webSessionToken, forKey: .webSessionToken)
        try container.encode(endpoint, forKey: .endpoint)
        try container.encode(runtimeStatus, forKey: .runtimeStatus)
        try container.encode(activeSessions, forKey: .activeSessions)
        try container.encode(totalSessions, forKey: .totalSessions)
        try container.encode(udpPacketsUp, forKey: .udpPacketsUp)
        try container.encode(udpPacketsDown, forKey: .udpPacketsDown)
        try container.encode(udpInvalidPackets, forKey: .udpInvalidPackets)
        try container.encode(tcpRelayFailures, forKey: .tcpRelayFailures)
        try container.encode(lootPayloadsSkipped, forKey: .lootPayloadsSkipped)
        // 诊断页的「目标地址」块（以及诊断页那份 pretty JSON）读的就是这两个键。
        try container.encode(destinations, forKey: .destinations)
        try container.encode(destinationsTotal, forKey: .destinationsTotal)
        if let lastHealthCheck {
            try container.encode(BattleBridge.timestampFormatter.string(from: lastHealthCheck), forKey: .lastHealthCheck)
        }
        try container.encodeIfPresent(error, forKey: .error)
    }

    static let idle = ReceiverStatus()
}

/// Configuration payload (INTERFACES.md 2) handed to `battle_proxy_start`.
/// Built with the same keys the Rust serde struct expects.
struct ReceiverConfig: Codable, Equatable, Sendable {
    struct EndpointBlock: Codable, Equatable, Sendable {
        struct Ports: Codable, Equatable, Sendable {
            var range: [UInt16]
        }
        var interface: String
        var ports: Ports
    }

    struct TransportBlock: Codable, Equatable, Sendable {
        struct SOCKS5: Codable, Equatable, Sendable {
            var tcpConnect: Bool
            var udpAssociate: Bool
            var udpRelayMode: String

            enum CodingKeys: String, CodingKey {
                case tcpConnect = "tcp_connect"
                case udpAssociate = "udp_associate"
                case udpRelayMode = "udp_relay_mode"
            }
        }
        var socks5: SOCKS5
        var udpNATMapping: String

        enum CodingKeys: String, CodingKey {
            case socks5
            case udpNATMapping = "udp_nat_mapping"
        }
    }

    struct DiagnosticsBlock: Codable, Equatable, Sendable {
        var protocolCapture: Bool
        var maxCaptureMB: Int
        var maxCaptureSeconds: Int

        enum CodingKeys: String, CodingKey {
            case protocolCapture = "protocol_capture"
            case maxCaptureMB = "max_capture_mb"
            case maxCaptureSeconds = "max_capture_seconds"
        }
    }

    struct CardBlock: Codable, Equatable, Sendable {
        var code: String?
        var activationURL: String

        enum CodingKeys: String, CodingKey {
            case code
            case activationURL = "activation_url"
        }
    }

    var dataDirectory: String
    var brand: String
    /// app 包内雷达前端目录的**绝对路径**。
    ///
    /// 必须显式传：Rust 侧 `embed::web_root()` 的默认值是 `env!("CARGO_MANIFEST_DIR")/../web`，
    /// 那是**编译机器**上的路径（CI 上就是 /Users/runner/work/...），在手机上不存在，
    /// 于是服务端只能返回 "radar page missing" 的占位页 —— 真机上表现为
    /// "雷达页面异常：HTML 已载入但地图前端未完成挂载"。
    var webRoot: String?
    var endpoint: EndpointBlock
    var transport: TransportBlock
    var sessionModel: String
    var parserAsync: Bool
    var readOnlyRadar: Bool
    var lootParsingEnabled: Bool
    var collectionPolicy: String
    var diagnostics: DiagnosticsBlock
    var adminToken: String
    var card: CardBlock

    enum CodingKeys: String, CodingKey {
        case dataDirectory = "data_directory"
        case brand, endpoint, transport
        case webRoot = "web_root"
        case sessionModel = "session_model"
        case parserAsync = "parser_async"
        case readOnlyRadar = "read_only_radar"
        case lootParsingEnabled = "loot_parsing_enabled"
        case collectionPolicy = "collection_policy"
        case diagnostics
        case adminToken = "admin_token"
        case card
    }

    /// The contract default port window: 2025...2045 inclusive.
    static let defaultPortRange: [UInt16] = [2025, 2045]

    /// app 包里的雷达前端目录。XcodeGen 用 `type: folder` 把它整体拷进 bundle 根，
    /// 所以就是 `<bundle>/web`。找不到时退回 nil，交给 Rust 用它自己的兜底逻辑
    /// （那时诊断页会直接显示它尝试过的路径，便于定位）。
    static var bundledWebRoot: String? {
        let path = Bundle.main.bundlePath + "/web"
        return FileManager.default.fileExists(atPath: path + "/index.html") ? path : nil
    }

    /// Canonical configuration for this build. Every field is explicit so the
    /// Rust side never has to fall back to a default that the UI does not know
    /// about.
    static func standard(
        dataDirectory: String,
        brand: String,
        adminToken: String,
        activationURL: String = "https://license.invalid/api/activate",
        portRange: [UInt16] = ReceiverConfig.defaultPortRange,
        webRoot: String? = ReceiverConfig.bundledWebRoot
    ) -> ReceiverConfig {
        ReceiverConfig(
            dataDirectory: dataDirectory,
            brand: brand,
            webRoot: webRoot,
            endpoint: EndpointBlock(
                interface: "0.0.0.0",
                ports: EndpointBlock.Ports(range: portRange)
            ),
            transport: TransportBlock(
                socks5: TransportBlock.SOCKS5(
                    tcpConnect: true,
                    udpAssociate: true,
                    udpRelayMode: "per_association_ephemeral"
                ),
                udpNATMapping: "per_client_endpoint_isolated"
            ),
            sessionModel: "one_port_one_player",
            parserAsync: true,
            readOnlyRadar: true,
            lootParsingEnabled: true,
            collectionPolicy: "enabled_only",
            diagnostics: DiagnosticsBlock(
                protocolCapture: false,
                maxCaptureMB: 64,
                maxCaptureSeconds: 600
            ),
            adminToken: adminToken,
            card: CardBlock(code: nil, activationURL: activationURL)
        )
    }
}

/// Error surface for the bridge. `rawPayload` always carries whatever the core
/// returned so the failure screen can show the untouched string.
enum BattleBridgeError: LocalizedError {
    case invalidUTF8(String)
    case startFailed(phase: String, message: String, rawPayload: String)
    case adminRejected(rawPayload: String)
    case bridgeUnavailable(String)

    var errorDescription: String? {
        switch self {
        case .invalidUTF8(let api):
            return "\(api) 返回了非 UTF-8 数据"
        case .startFailed(let phase, let message, _):
            return "接收器启动失败（phase=\(phase)）：\(message)"
        case .adminRejected:
            return "本机管理接口拒绝了该操作"
        case .bridgeUnavailable(let api):
            return "\(api) 未返回数据（连接库符号缺失）"
        }
    }

    var rawPayload: String {
        switch self {
        case .invalidUTF8(let api):
            return "invalid utf-8 from \(api)"
        case .startFailed(_, _, let rawPayload):
            return rawPayload
        case .adminRejected(let rawPayload):
            return rawPayload
        case .bridgeUnavailable(let api):
            return "no payload returned by \(api)"
        }
    }
}

/// Admin route names accepted by `battle_proxy_admin` (INTERFACES.md 4).
enum BattleAdminRoute: String, Sendable {
    case diag = "/api/admin/diag"
    case loot = "/api/admin/loot"
    case sessionReset = "/api/admin/session/reset"
    case announcement = "/api/admin/announcement"
    case captureStart = "/api/admin/capture/start"
    case captureStop = "/api/admin/capture/stop"
    case captureDownload = "/api/admin/capture/download"
    case shutdown = "/api/admin/shutdown"
}

// MARK: - Bridge

/// Thin, allocation-safe wrapper over the C ABI.
final class BattleBridge {

    static let shared = BattleBridge()

    /// Status payloads are ISO8601. The core emits fractional seconds, so both
    /// formatters are consulted before giving up.
    static let timestampFormatter: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter
    }()

    private static let plainTimestampFormatter: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter
    }()

    static func decodeTimestamp(_ raw: String?) -> Date? {
        guard let raw, !raw.isEmpty else { return nil }
        if let date = timestampFormatter.date(from: raw) { return date }
        return plainTimestampFormatter.date(from: raw)
    }

    private let decoder: JSONDecoder

    private init() {
        decoder = JSONDecoder()
    }

    // MARK: Version

    /// Static build string from the linked library, e.g. "2.3.7-r39".
    var version: String {
        guard let pointer = battle_proxy_version() else { return "unknown" }
        let value = String(cString: pointer)
        return value.isEmpty ? "unknown" : value
    }

    // MARK: Lifecycle (blocking - call from a background task)

    /// Consumes the C string, frees it, then decodes. Throws with the raw
    /// payload intact when the core reports phase=failed.
    private func withResult<T>(
        api: String,
        producedBy body: () -> UnsafeMutablePointer<CChar>?,
        transform: (OrderedJSON) throws -> T
    ) throws -> T {
        guard let raw = body() else {
            throw BattleBridgeError.bridgeUnavailable(api)
        }
        // The buffer belongs to the Rust core until it is handed back, so the
        // Swift copy has to be taken before freeing.
        let rawString = String(cString: raw)
        battle_proxy_free_string(raw)

        guard let json = OrderedJSON(rawString) else {
            throw BattleBridgeError.invalidUTF8(api)
        }
        return try transform(json)
    }

    func start(config: ReceiverConfig) throws -> ReceiverStatus {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let configJSON = String(data: try encoder.encode(config), encoding: .utf8) ?? "{}"

        return try withResult(
            api: "battle_proxy_start",
            producedBy: { configJSON.withCString { battle_proxy_start($0) } },
            transform: { json in
                let status = try self.decoder.decode(ReceiverStatus.self, from: json.data)
                guard status.phase == .running else {
                    throw BattleBridgeError.startFailed(
                        phase: status.phase.rawValue,
                        message: status.error ?? status.runtimeStatus.failedReason ?? "核心未返回原因",
                        rawPayload: json.rawString
                    )
                }
                return status
            }
        )
    }

    func stop() throws -> ReceiverStatus {
        try withResult(
            api: "battle_proxy_stop",
            producedBy: { battle_proxy_stop() },
            transform: { try self.decoder.decode(ReceiverStatus.self, from: $0.data) }
        )
    }

    /// Non-throwing poll used by the 2 s health monitor: a transient decode
    /// failure must never tear down the UI, so nil simply means "keep the
    /// previous snapshot".
    func status() -> ReceiverStatus? {
        guard let raw = battle_proxy_status() else { return nil }
        defer { battle_proxy_free_string(raw) }
        let rawString = String(cString: raw)
        guard let json = OrderedJSON(rawString) else { return nil }
        return try? decoder.decode(ReceiverStatus.self, from: json.data)
    }

    /// Local-only admin call. Returns the raw JSON body the core produced.
    ///
    /// The C strings are duplicated with `strdup` so the three pointer
    /// arguments can be handed over without nesting `withCString` scopes (which
    /// Swift rejects as overlapping exclusive access to the same pointer).
    @discardableResult
    func admin(route: BattleAdminRoute, token: String, body: String? = nil) throws -> OrderedJSON {
        try withResult(
            api: "battle_proxy_admin",
            producedBy: {
                let tokenCopy = strdup(token)
                let pathCopy = strdup(route.rawValue)
                let bodyCopy = body.flatMap { strdup($0) }
                defer {
                    free(tokenCopy)
                    free(pathCopy)
                    if let bodyCopy { free(bodyCopy) }
                }
                return battle_proxy_admin(tokenCopy, pathCopy, bodyCopy)
            },
            transform: { json in
                let ok = json.boolValue(forKey: "ok")
                // Only an explicit `"ok": false` is a rejection; routes that
                // answer with a plain payload (diag, capture/download) pass.
                if ok == false {
                    throw BattleBridgeError.adminRejected(rawPayload: json.rawString)
                }
                return json
            }
        )
    }

    // MARK: Utilities shared with the UI layer

    /// 32 lowercase hex characters, used as the loopback admin token.
    static func randomHexToken(byteCount: Int = 16) -> String {
        var bytes = [UInt8](repeating: 0, count: max(1, byteCount))
        let status = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        if status != errSecSuccess {
            // Deterministic fallback so startup can never dead-end on entropy.
            let stamp = UInt64(Date().timeIntervalSince1970 * 1_000)
            for index in bytes.indices {
                bytes[index] = UInt8(truncatingIfNeeded: (stamp &* UInt64(index &+ 1)) >> (index % 5))
            }
        }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }

    /// Best-effort LAN IPv4 for the pairing panel (en0 preferred, loopback and
    /// link-local excluded).
    static func localIPv4Address() -> String? {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return nil }
        defer { freeifaddrs(head) }

        var candidates: [(name: String, address: String)] = []
        var cursor: UnsafeMutablePointer<ifaddrs>? = first
        while let interface = cursor {
            defer { cursor = interface.pointee.ifa_next }
            guard let socketAddress = interface.pointee.ifa_addr else { continue }
            guard socketAddress.pointee.sa_family == UInt8(AF_INET) else { continue }

            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let result = getnameinfo(
                socketAddress,
                socklen_t(socketAddress.pointee.sa_len),
                &host,
                socklen_t(host.count),
                nil,
                0,
                NI_NUMERICHOST
            )
            guard result == 0 else { continue }
            let address = String(cString: host)
            guard !address.isEmpty, address != "127.0.0.1", !address.hasPrefix("169.254.") else { continue }
            let name = String(cString: interface.pointee.ifa_name)
            candidates.append((name: name, address: address))
        }

        if let en0 = candidates.first(where: { $0.name == "en0" }) { return en0.address }
        if let first = candidates.first { return first.address }
        return nil
    }

    /// Application support style directory for receiver state, created on demand.
    static func makeDataDirectory() -> String {
        let base = FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
        let directory = base.appendingPathComponent("BattleReceiver", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory.path
    }
}

// MARK: - OrderedJSON

/// Small helper that keeps the raw text around (diagnostics sheet shows the
/// untouched payload) while still allowing `JSONSerialization` based probing.
struct OrderedJSON {
    let rawString: String
    let data: Data
    let object: Any?

    init?(_ rawString: String) {
        guard let data = rawString.data(using: .utf8) else { return nil }
        self.rawString = rawString
        self.data = data
        self.object = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
    }

    func boolValue(forKey key: String) -> Bool? {
        guard let dictionary = object as? [String: Any] else { return nil }
        if let value = dictionary[key] as? Bool { return value }
        if let value = dictionary[key] as? NSNumber { return value.boolValue }
        if let value = dictionary[key] as? String { return (value as NSString).boolValue }
        return nil
    }

    /// Pretty printed form for the diagnostics sheet; falls back to the raw text.
    var prettyString: String {
        guard
            let object,
            JSONSerialization.isValidJSONObject(object),
            let pretty = try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys]),
            let text = String(data: pretty, encoding: .utf8)
        else {
            return rawString
        }
        return text
    }
}
