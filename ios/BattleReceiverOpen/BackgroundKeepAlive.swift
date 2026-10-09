//
//  BackgroundKeepAlive.swift
//  BattleReceiverOpen
//
//  A 机的后台保活：按参考实现（docs/REFERENCE_MYRADERPRO.md §4.3）用「静音 PCM 循环
//  播放」换取后台存活时间。
//
//  为什么必须这么干：iOS 会在 App 切到后台后挂起进程，A 机一被挂起，B 机的
//  SOCKS5/TCP + UDP 中继立刻全断（真机症状：A 机一锁屏/切后台，B 机"全网断"，
//  只要 A 机屏幕亮着就一切正常）。系统允许的后台例外里音频是最廉价可靠的一条：
//  Info.plist 的 UIBackgroundModes=[audio] + .playback 音频会话 + 持续播放**静音**
//  缓冲，进程就能一直活着继续转发。
//
//  取舍（照抄参考实现，不是随手选的）：
//    * `.playback` 会**打断用户自己的音乐/播客**（其他 App 的声音会被停掉或压低）。
//      这是有意为之：保活优先于"不打扰用户音乐"。改成 `.mixWithOthers` 看起来更友好，
//      但系统在切后台时更容易直接停掉这条音频，保活就不成立了。
//    * 屏幕常亮由 `setIdleTimerDisabled` 负责（见 ReceiverModel），它只解决"亮屏时
//      被系统挂起"；切后台/锁屏仍然要靠这里的静音播放。
//    * 播的是全零 PCM：没有实际声音。刻意**不**把 mainMixerNode 音量调成 0 ——
//      音量为 0 时系统可能认为没有真实渲染而回收这条音频通道，保活就失效了。
//
//  线程：AVAudioSession / AVAudioEngine 的调用都在主线程（本类是 @MainActor），
//  调用方 ReceiverModel 同样是 @MainActor，不需要额外队列。
//

import AVFoundation
import Foundation

/// 归一化以后的通知事件（文件级类型：只搬可以跨隔离域的基础类型，不把 `Notification`
/// 送进 Task）。
enum KeepAliveEvent: Sendable {
    case interruptionBegan
    case interruptionEnded
    case routeChanged(UInt)
    case mediaServicesReset
    case mediaServicesLost
}

@MainActor
final class BackgroundKeepAlive {

    /// 当前是否真的在渲染静音音频（诊断页显示用）。
    private(set) var isActive = false
    /// 最近一次失败原因（只用于诊断，不弹窗、不中断接收器）。
    private(set) var lastError: String?

    /// 调用方希望保活开着（running 阶段）—— 与"当前是否真的在播"分开，
    /// 因为中断/路由变化会临时停掉播放，之后要能自动恢复。
    private var wantsRunning = false
    private var graphReady = false
    private var observers: [NSObjectProtocol] = []

    private var engine = AVAudioEngine()
    private var player = AVAudioPlayerNode()

    /// 静音缓冲的采样率。44.1 kHz 是系统最不可能拒绝的档位，且 1 秒 1 循环很省电。
    private static let sampleRate: Double = 44_100

    // MARK: - Lifecycle

    /// 开启保活（幂等）：激活音频会话 + 起静音循环。失败时静默降级 —— 接收器本身照常工作，
    /// 只是切后台会被系统挂起（这正是保活要解决的问题，但不该让 App 崩或卡住）。
    func start() {
        wantsRunning = true
        installObserversIfNeeded()

        guard activateSession() else { return }
        startLoop()
    }

    /// 关闭保活：停播放 + 会话设为 inactive（把音频交还系统，用户音乐可以恢复播放）。
    func stop() {
        wantsRunning = false

        player.stop()
        engine.stop()
        // `stop()` 会清掉已排队的缓冲，所以下次开启必须重建图并重新 scheduleBuffer。
        graphReady = false
        isActive = false

        do {
            try AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
        } catch {
            // 会话本来就没激活时这里必然抛错，不是故障：忽略。
        }
    }

    /// 从后台回到前台、或中断/路由变化之后调用：只要还"想跑"就把静音循环重新拉起来。
    func resumeIfNeeded() {
        guard wantsRunning else { return }
        guard activateSession() else { return }
        startLoop()
    }

    // MARK: - Audio session

    /// 激活 `.playback` 会话。返回 false 表示本次没法保活（记录原因后由调用方静默降级）。
    private func activateSession() -> Bool {
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default, options: [])
            try session.setActive(true)
            lastError = nil
            return true
        } catch {
            lastError = "音频会话激活失败：\(error.localizedDescription)"
            isActive = false
            return false
        }
    }

    // MARK: - Silent loop

    /// 起（或重启）静音循环。图没建过就先建。
    private func startLoop() {
        if !graphReady {
            graphReady = buildGraph()
        }
        guard graphReady else { return }

        do {
            if !engine.isRunning {
                try engine.start()
            }
            if !player.isPlaying {
                player.play()
            }
            isActive = engine.isRunning && player.isPlaying
            if isActive {
                lastError = nil
            }
        } catch {
            lastError = "静音播放启动失败：\(error.localizedDescription)"
            isActive = false
        }
    }

    /// 重建音频图（engine/player 都要新的：`player.stop()` 已清空缓冲，媒体服务重启后
    /// 旧对象也全部失效）。成功返回 true。
    private func buildGraph() -> Bool {
        engine.stop()
        engine = AVAudioEngine()
        player = AVAudioPlayerNode()
        engine.attach(player)

        guard
            let format = AVAudioFormat(standardFormatWithSampleRate: Self.sampleRate, channels: 1),
            let silent = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(Self.sampleRate))
        else {
            lastError = "无法创建静音 PCM 缓冲"
            return false
        }

        silent.frameLength = silent.frameCapacity
        // 全零 = 真静音。AVAudioPCMBuffer 新分配的数据本来就是 0，这里显式写一遍，
        // 免得以后有人改动分配方式时把未初始化内存播出去。
        if let channels = silent.floatChannelData {
            for channel in 0..<Int(format.channelCount) {
                let samples = channels[channel]
                for index in 0..<Int(silent.frameLength) {
                    samples[index] = 0
                }
            }
        }

        engine.connect(player, to: engine.mainMixerNode, format: format)
        player.scheduleBuffer(silent, at: nil, options: .loops, completionHandler: nil)
        return true
    }

    /// 临时停播（中断开始、媒体服务丢失时用）：保留 `wantsRunning`，等恢复时再拉起来。
    private func pauseLoop() {
        player.pause()
        engine.pause()
        isActive = false
    }

    // MARK: - Notifications (中断 / 路由变化 / 媒体服务重启)

    /// 只装一次；`object: nil` 是刻意的：这些通知的 object 在系统各版本里不一定等于
    /// session 实例，传具体对象有可能一条都收不到。
    private func installObserversIfNeeded() {
        guard observers.isEmpty else { return }

        let center = NotificationCenter.default
        let names: [Notification.Name] = [
            AVAudioSession.interruptionNotification,
            AVAudioSession.routeChangeNotification,
            AVAudioSession.mediaServicesWereResetNotification,
            AVAudioSession.mediaServicesWereLostNotification,
        ]

        for name in names {
            let token = center.addObserver(forName: name, object: nil, queue: .main) { [weak self] note in
                let event = BackgroundKeepAlive.event(for: note)
                Task { @MainActor in
                    guard let event else { return }
                    self?.handle(event)
                }
            }
            observers.append(token)
        }
    }

    /// 通知 → 事件的纯函数（nonisolated：观察闭包不在主 actor 上）。
    nonisolated static func event(for note: Notification) -> KeepAliveEvent? {
        switch note.name {
        case AVAudioSession.interruptionNotification:
            let raw = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt) ?? 0
            guard let type = AVAudioSession.InterruptionType(rawValue: raw) else { return nil }
            return type == .began ? .interruptionBegan : .interruptionEnded
        case AVAudioSession.routeChangeNotification:
            let raw = (note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt) ?? 0
            return .routeChanged(raw)
        case AVAudioSession.mediaServicesWereResetNotification:
            return .mediaServicesReset
        case AVAudioSession.mediaServicesWereLostNotification:
            return .mediaServicesLost
        default:
            return nil
        }
    }

    private func handle(_ event: KeepAliveEvent) {
        switch event {
        case .interruptionBegan:
            // 来电/闹钟/Siri 抢占了音频：系统已经停掉播放，这里只记状态，别报错。
            pauseLoop()
        case .interruptionEnded:
            // 中断结束要显式重新激活会话并重排缓冲，否则永远不会自己恢复。
            resumeIfNeeded()
        case .routeChanged(let raw):
            guard let reason = AVAudioSession.RouteChangeReason(rawValue: raw) else { return }
            switch reason {
            // 拔/插耳机、切蓝牙、类别变化都会让系统停掉当前播放：只要还想跑就重新拉起来。
            case .oldDeviceUnavailable, .newDeviceAvailable, .categoryChange, .override:
                resumeIfNeeded()
            default:
                break
            }
        case .mediaServicesReset:
            // 音频服务整体重启：旧的 engine/player 全部失效，必须重建。
            graphReady = false
            resumeIfNeeded()
        case .mediaServicesLost:
            isActive = false
        }
    }
}
