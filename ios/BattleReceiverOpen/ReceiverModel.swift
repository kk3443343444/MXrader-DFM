//
//  ReceiverModel.swift
//  BattleReceiverOpen
//
//  ObservableObject driving the whole shell (the original binary's obfuscated
//  `X0B2`). Owns the lifecycle of the Rust receiver, the radar web session and
//  the LAN pairing metadata.
//
//  Threading contract:
//    * the class is @MainActor, so every @Published mutation is main thread safe
//    * battle_proxy_start/stop/admin block, so they run inside a detached task
//      through BattleBridge (which only holds a JSONDecoder, so it is safe to
//      hand to another executor)
//    * status polling happens on a background task every 2 seconds
//

import Foundation
import Combine
// ScenePhase（以及视图层直接读的几个 URL 类型）来自 SwiftUI；这个文件只用到
// 个别类型，所以单独 import 而不是把模型塞进视图层。
import SwiftUI

@MainActor
final class ReceiverModel: ObservableObject {

    // MARK: - Sheet routing

    enum PresentedSheet: String, Identifiable {
        case pairing
        case diagnostics
        var id: String { rawValue }
    }

    // MARK: - Published state

    @Published private(set) var phase: ReceiverPhase = .idle
    @Published private(set) var endpoint: ReceiverEndpoint = .empty
    @Published private(set) var runtimeStatus: RuntimeStatus = .unknown
    @Published private(set) var webURL: URL?
    @Published private(set) var webSessionToken: String?
    @Published var presentedSheet: PresentedSheet?
    @Published private(set) var lastHealthCheck: Date?
    @Published private(set) var receiverFailureMessage: String?
    @Published var webReloadToken: Int = 0
    /// Bumped when the user asks for "雷达设置 → 网络日志"; BattleWebView turns
    /// the change into a JS navigation request against the local radar page.
    @Published var battleLogToken: Int = 0

    /// Full counters snapshot, kept fresh by the 2 s health monitor and by the
    /// diagnostics sheet.
    @Published private(set) var statusSnapshot: ReceiverStatus = .idle
    @Published private(set) var lastStatusError: String?

    /// 后台保活是否真的在播静音音频（诊断页显示用）。
    @Published private(set) var keepAliveActive = false

    /// Set once a live radar page is available; keeps `start()` idempotent
    /// across scene reactivation.
    private(set) var hasBooted = false

    /// False while the app is in the background.
    private(set) var isSceneActive = true

    // MARK: - Identity

    let brand: String
    let appVersion: String
    let appBuild: String
    let dataDirectory: String

    // MARK: - Derived values used across the UI

    /// Build string of the linked Rust library, e.g. "2.3.7-r39".
    var receiverVersion: String { BattleBridge.shared.version }

    var socksPort: UInt16 {
        statusSnapshot.socksPort ?? statusSnapshot.primaryPort ?? 0
    }

    var webPort: UInt16 {
        statusSnapshot.webPort ?? statusSnapshot.primaryPort ?? 0
    }

    /// Number of SOCKS5 associations currently relayed - the counter the release
    /// notes call the "open handle".
    var openHandle: Int { statusSnapshot.activeSessions }

    var localIPCandidate: String? { BattleBridge.localIPv4Address() }

    var displayAddress: String {
        endpoint.displayAddress ?? localIPCandidate ?? "0.0.0.0"
    }

    var isRunning: Bool { phase == .running }

    // MARK: - Pairing strings for device B

    /// `socks5://<lan-ip>:<port>` as shown in the pairing sheet.
    var socksURL: String {
        if let reported = endpoint.socksURL, !reported.isEmpty { return reported }
        let host = endpoint.displayAddress ?? localIPCandidate ?? "127.0.0.1"
        let port = socksPort == 0 ? 2025 : socksPort
        return "socks5://\(host):\(port)"
    }

    /// `http://<lan-ip>:<port>/battle.html?brand=mx` - readable from the other
    /// device on the same Wi-Fi.
    var radarDisplayURL: String {
        if let reported = endpoint.radarDisplayAddress, !reported.isEmpty { return reported }
        let host = endpoint.displayAddress ?? localIPCandidate ?? "127.0.0.1"
        let port = webPort == 0 ? 2025 : webPort
        return "http://\(host):\(port)/battle.html?brand=\(brand)"
    }

    /// 当前雷达页地址（视图层直接读这个实例属性）。
    ///
    /// 真正的计算放在下面的 `static func browserURL(from:brand:)` 里，这样没有模型
    /// 实例时（例如启动早期或测试）也能算；视图侧只认这个属性，两边不要各写一份。
    var browserURL: URL? {
        Self.browserURL(from: endpoint, brand: brand)
    }

    /// Hiddify / sing-box share profile served by the Rust HTTP layer.
    var hiddifyProfileURL: String {
        let host = endpoint.displayAddress ?? localIPCandidate ?? "127.0.0.1"
        let port = webPort == 0 ? 2025 : webPort
        return "http://\(host):\(port)/api/socks5/hiddify.json"
    }

    // MARK: - Private

    private let bridge = BattleBridge.shared
    /// 监听 phase 变化以同步"是否禁止息屏"（见 init 里的说明）。
    private var phaseSink: AnyCancellable?
    /// 后台保活（音频静音播放，见 BackgroundKeepAlive.swift）。
    private let keepAlive = BackgroundKeepAlive()
    private let advertiser: BonjourAdvertiser
    private var healthTask: Task<Void, Never>?
    private var startTask: Task<Void, Never>?
    private var pendingStart: Task<ReceiverStatus, Error>?
    /// Total splash budget. The core promises `phase=running` within 5 s
    /// (INTERFACES.md 1), so exceeding it is reported as a stall.
    static let startupBudget: TimeInterval = 5.0

    // MARK: - Init

    init(
        brand: String = BattleBrand.current,
        appVersion: String = Bundle.main.battleMarketingVersion,
        appBuild: String = Bundle.main.battleBuildNumber,
        dataDirectory: String = BattleBridge.makeDataDirectory()
    ) {
        self.brand = brand
        self.appVersion = appVersion
        self.appBuild = appBuild
        self.dataDirectory = dataDirectory
        self.advertiser = BonjourAdvertiser(brand: brand)
        // 运行期间禁止自动息屏。
        //
        // 原因：Info.plist 现在带 UIBackgroundModes=[audio]（配合静音播放），但**亮屏**
        // 时的挂起由系统按自动锁屏时间决定 —— 默认只有 30 秒，而玩家必然要把注意力放在
        // B 机上，不锁屏这件事必须由代码保证；切后台/锁屏则由 BackgroundKeepAlive 兜住。
        phaseSink = $phase
            .removeDuplicates()
            .sink { [weak self] newPhase in
                // 用 Task 跳到主 actor：@Published 的 sink 虽然在主线程触发，
                // 但闭包本身不是主 actor 隔离的，直接访问 UIApplication 会在
                // Swift 6 严格并发下报错（Swift 5 只是警告，但这个写法两边都安全）。
                // AVAudioSession 同样只能在主线程调用（见 BackgroundKeepAlive 的注释）。
                Task { @MainActor in
                    guard let self else { return }
                    UIApplication.shared.isIdleTimerDisabled = (newPhase == .running)

                    // 后台保活与 phase 联动：只有接收器真的在跑才播静音音频，
                    // 其它阶段（idle/starting/failed/stopping）立刻停掉并把音频会话交还系统。
                    if newPhase == .running {
                        self.keepAlive.start()
                    } else {
                        self.keepAlive.stop()
                    }
                    self.keepAliveActive = self.keepAlive.isActive
                }
            }
        preflight()
    }

    deinit {
        healthTask?.cancel()
        startTask?.cancel()
        pendingStart?.cancel()
    }

    // MARK: - Preflight

    /// Cheap synchronous checks run before the native core is touched: writable
    /// data directory and a resolvable LAN address.
    @discardableResult
    func preflight() -> Bool {
        phase = .preflight

        let fileManager = FileManager.default
        if !fileManager.fileExists(atPath: dataDirectory) {
            try? fileManager.createDirectory(atPath: dataDirectory, withIntermediateDirectories: true)
        }
        guard fileManager.isWritableFile(atPath: dataDirectory) else {
            receiverFailureMessage = "数据目录不可写：\(dataDirectory)"
            phase = .failed
            return false
        }
        guard BattleBridge.localIPv4Address() != nil else {
            receiverFailureMessage = "未检测到局域网 IPv4 地址，请先让本机连接 Wi-Fi。"
            phase = .failed
            return false
        }
        return true
    }

    // MARK: - Startup

    /// Boots the receiver. Idempotent: a second call while starting/running is a
    /// no-op, which keeps SwiftUI view re-evaluation from double-starting.
    func start() {
        guard !hasBooted, phase != .starting, phase != .running else { return }
        guard preflight() else { return }

        phase = .starting
        receiverFailureMessage = nil
        lastStatusError = nil
        refreshEndpoint()

        let config = ReceiverConfig.standard(
            dataDirectory: dataDirectory,
            brand: brand,
            adminToken: BattleBridge.randomHexToken()
        )

        startTask?.cancel()
        startTask = Task { [weak self] in
            guard let self else { return }
            let outcome = await self.runStart(config: config)
            guard !Task.isCancelled else { return }
            await self.finishStart(outcome)
        }
    }

    private enum StartOutcome: Sendable {
        case success(ReceiverStatus)
        case failure(Error)
        case timeout
    }

    /// Runs the blocking C call off the main actor and enforces the 5 s budget.
    private func runStart(config: ReceiverConfig) async -> StartOutcome {
        let bridge = self.bridge
        let task = Task.detached(priority: .userInitiated) { () -> ReceiverStatus in
            try bridge.start(config: config)
        }
        pendingStart = task

        let timeoutTask = Task { () -> StartOutcome in
            try? await Task.sleep(nanoseconds: UInt64(Self.startupBudget * 1_000_000_000))
            return .timeout
        }

        let outcome = await withTaskGroup(of: StartOutcome.self, returning: StartOutcome.self) { group in
            group.addTask {
                do {
                    return .success(try await task.value)
                } catch {
                    return .failure(error)
                }
            }
            group.addTask { await timeoutTask.value }

            let first = await group.next() ?? .timeout
            group.cancelAll()
            return first
        }

        pendingStart = nil
        if case .timeout = outcome {
            // The core may still bind late; the health monitor will adopt it.
            // Only the splash budget expired here.
            return .timeout
        }
        return outcome
    }

    private func finishStart(_ outcome: StartOutcome) async {
        switch outcome {
        case .success(let snapshot):
            apply(status: snapshot)
            beginHealthMonitoring()
            if let port = snapshot.primaryPort ?? snapshot.socksPort, port > 0 {
                advertiser.start(port: port)
            }
            hasBooted = webURL != nil
        case .failure(let error):
            hasBooted = false
            fail(with: error)
        case .timeout:
            hasBooted = false
            receiverFailureMessage = "启动超时：battle_proxy_start 未在 \(Int(Self.startupBudget)) 秒内返回（端口段 2025–2045 可能被占用）。"
            phase = .failed
        }
    }

    /// Called by the splash screen when its own 5 s timer fires.
    func handleSplashTimeout() {
        pendingStart?.cancel()
        pendingStart = nil

        if phase == .running { return }

        // The web layer answered just after the budget: trust it and keep going.
        if let snapshot = bridge.status(), snapshot.phase == .running {
            apply(status: snapshot)
            hasBooted = webURL != nil
            beginHealthMonitoring()
            return
        }
        if webURL != nil, phase.isTransient {
            hasBooted = true
            return
        }

        phase = .failed
        hasBooted = false
        receiverFailureMessage = "启动超时：接收器未在 5 秒内就绪，请确认 2025–2045 端口段未被其他应用占用。"
    }

    // MARK: - Stop / retry

    func stop() {
        healthTask?.cancel()
        healthTask = nil
        startTask?.cancel()
        startTask = nil
        pendingStart?.cancel()
        pendingStart = nil
        advertiser.stop()

        phase = .stopping
        let bridge = self.bridge
        Task.detached(priority: .utility) {
            _ = try? bridge.stop()
        }
    }

    /// Full recovery path used by ReceiverFailureView and the diagnostics sheet.
    func retry() {
        hasBooted = false
        receiverFailureMessage = nil
        stop()
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 400_000_000)
            guard let self else { return }
            self.phase = .idle
            self.start()
        }
    }

    // MARK: - Health monitoring

    /// Polls `battle_proxy_status()` every 2 s and republishes the snapshot.
    func beginHealthMonitoring() {
        guard healthTask == nil else { return }
        let bridge = self.bridge
        healthTask = Task.detached(priority: .utility) { [weak self] in
            while !Task.isCancelled {
                if let snapshot = bridge.status() {
                    await MainActor.run { self?.apply(status: snapshot) }
                }
                try? await Task.sleep(nanoseconds: 2_000_000_000)
            }
            await MainActor.run { [weak self] in
                self?.healthTask = nil
            }
        }
    }

    /// Applies a status snapshot on the main actor.
    func apply(status snapshot: ReceiverStatus) {
        statusSnapshot = snapshot
        phase = snapshot.phase
        runtimeStatus = snapshot.runtimeStatus
        lastHealthCheck = snapshot.lastHealthCheck ?? Date()

        if let message = snapshot.error {
            lastStatusError = message
        } else if let reason = snapshot.runtimeStatus.failedReason {
            lastStatusError = reason
        }

        if let url = Self.browserURL(from: snapshot.endpoint, brand: brand) {
            webURL = url
        }
        if let token = snapshot.webSessionToken, !token.isEmpty {
            webSessionToken = token
        }
        if !snapshot.endpoint.displayAddress.isNilOrEmpty {
            endpoint = snapshot.endpoint
        }

        switch snapshot.phase {
        case .failed:
            if let message = snapshot.error ?? snapshot.runtimeStatus.failedReason {
                receiverFailureMessage = message
            }
            hasBooted = false
        case .running:
            if hasBooted == false { hasBooted = webURL != nil }
            if advertiser.isAdvertising == false,
               let port = snapshot.primaryPort ?? snapshot.socksPort, port > 0 {
                advertiser.start(port: port)
            }
        default:
            break
        }
    }

    /// Force a `battle_proxy_status()` round trip (diagnostics refresh button,
    /// scene activation).
    func refreshStatusNow() {
        if let snapshot = bridge.status() {
            apply(status: snapshot)
        } else {
            lastStatusError = "battle_proxy_status() 未返回数据"
            refreshEndpoint()
        }
    }

    /// Re-reads the LAN address and re-derives the pairing strings.
    func refreshEndpoint() {
        let host = BattleBridge.localIPv4Address() ?? "127.0.0.1"
        let port = statusSnapshot.primaryPort ?? statusSnapshot.socksPort ?? 2025
        let reported = statusSnapshot.endpoint

        if reported.displayAddress.isNilOrEmpty {
            endpoint = ReceiverEndpoint(
                interface: "0.0.0.0",
                displayAddress: host,
                radarDisplayAddress: "http://\(host):\(port)/battle.html?brand=\(brand)",
                socksURL: "socks5://\(host):\(port)",
                radarURL: "http://127.0.0.1:\(port)/battle.html?brand=\(brand)"
            )
        } else {
            endpoint = reported
        }
        if let url = Self.browserURL(from: endpoint, brand: brand) {
            webURL = url
        }
    }

    /// Async spelling used by the pairing sheet's "刷新局域网地址" button.
    func refreshEndpoint() async {
        refreshStatusNow()
    }

    // MARK: - Web view coordination

    /// Bump the token handed to BattleWebView so the WKWebView reloads.
    func reloadWebView() {
        webReloadToken &+= 1
    }

    /// Ask the radar page to open its network log view
    /// (the diagnostics sheet's "雷达设置 → 网络日志" button).
    func openNetworkLog() {
        battleLogToken &+= 1
    }

    // MARK: - Scene phase

    /// Called from BattleReceiverApp's scenePhase handling. Keeping the state in
    /// the model lets the splash skip its animations and the monitor skip its
    /// polls while the app is in the background.
    func updateSceneActivity(_ scenePhase: ScenePhase) {
        switch scenePhase {
        case .active:
            isSceneActive = true
            if hasBooted || phase == .running {
                beginHealthMonitoring()
                refreshStatusNow()
                if advertiser.isAdvertising == false, socksPort > 0 {
                    advertiser.start(port: socksPort)
                }
            } else if phase.isTransient, phase != .starting {
                start()
            }
            // 后台保活：切回前台时把静音循环（重新）拉起来 —— 中断、路由变化（拔耳机）、
            // 以及媒体服务重启都会让播放停掉，这里做一次兜底恢复；不跑阶段则是空操作。
            if phase == .running {
                keepAlive.resumeIfNeeded()
                keepAliveActive = keepAlive.isActive
            }
        case .inactive:
            isSceneActive = false
        case .background:
            // The listener stays bound so device B keeps its tunnel; only the
            // radar page stops streaming.
            isSceneActive = false
        @unknown default:
            isSceneActive = false
        }
    }

    // MARK: - Admin channel (diagnostics sheet)

    /// Local-only admin route (INTERFACES.md 4). Returns nil and records the
    /// error when the route is rejected.
    @discardableResult
    func performAdmin(_ route: BattleAdminRoute) async -> OrderedJSON? {
        let bridge = self.bridge
        let token = webSessionToken ?? ""
        let result: Result<OrderedJSON, Error> = await Task.detached(priority: .utility) { () -> Result<OrderedJSON, Error> in
            do {
                return .success(try bridge.admin(route: route, token: token))
            } catch {
                return .failure(error)
            }
        }.value

        switch result {
        case .success(let json):
            refreshStatusNow()
            return json
        case .failure(let error):
            lastStatusError = (error as? BattleBridgeError)?.rawPayload ?? error.localizedDescription
            return nil
        }
    }

    /// Diagnostics sheet "重新启动接收服务" button.
    func restartReceiver() async {
        retry()
        // Give the core a beat to release the socket before the UI reports done.
        try? await Task.sleep(nanoseconds: 400_000_000)
    }

    // MARK: - Failure plumbing

    private func fail(with error: Error) {
        let raw = (error as? BattleBridgeError)?.rawPayload ?? error.localizedDescription

        if Self.looksLikePortExhaustion(raw) {
            receiverFailureMessage = "接收端口被其他应用占用（已自动尝试 2025–2045）。"
        } else {
            receiverFailureMessage = error.localizedDescription
        }
        lastStatusError = raw
        phase = .failed
        hasBooted = false
    }

    private static func looksLikePortExhaustion(_ raw: String) -> Bool {
        let lowered = raw.lowercased()
        let needles = ["port", "bind", "eaddrinuse", "address already in use", "端口", "占用"]
        return needles.contains { lowered.contains($0) }
    }

    // MARK: - Static helpers

    static func browserURL(from endpoint: ReceiverEndpoint, brand: String) -> URL? {
        if let raw = endpoint.radarURL, !raw.isEmpty, let url = URL(string: raw) { return url }
        let port = endpoint.radarDisplayAddress
            .flatMap { URL(string: $0)?.port }
            ?? 2025
        return URL(string: "http://127.0.0.1:\(port)/battle.html?brand=\(brand)")
    }
}

// MARK: - Small helpers

extension Optional where Wrapped == String {
    var isNilOrEmpty: Bool {
        switch self {
        case .none: return true
        case .some(let value): return value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
    }
}

// MARK: - Bundle helpers

extension Bundle {
    var battleMarketingVersion: String {
        (object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "2.3.7"
    }

    var battleBuildNumber: String {
        (object(forInfoDictionaryKey: "CFBundleVersion") as? String) ?? "32"
    }

    var battleDisplayName: String {
        (object(forInfoDictionaryKey: "CFBundleDisplayName") as? String) ?? "MXrader 三角洲"
    }

    var battleBrandVariant: String {
        (object(forInfoDictionaryKey: "BattleBrandVariant") as? String) ?? "mx"
    }
}
