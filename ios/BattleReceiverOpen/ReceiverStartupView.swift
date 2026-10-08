//
//  ReceiverStartupView.swift
//  BattleReceiverOpen
//
//  Splash / boot screen. The brand composition comes from BattleSplashView and
//  this file adds the boot chronology of the original app:
//    1. pick a free port inside 2025...2045 and bring the receiver up
//    2. bind the SOCKS5 TCP + UDP relay
//    3. wait for the embedded radar page
//    4. confirm the local network permission prompt
//
//  The whole screen is budgeted at `timeout` seconds (5 s by contract). When the
//  budget expires the view asks the model to finish or fail the boot; the model
//  is the single owner of the phase, so no view decides the outcome on its own.
//

import SwiftUI

struct ReceiverStartupView: View {

    @ObservedObject var model: ReceiverModel
    /// Total budget of the splash screen (5 s per the interface contract).
    var timeout: TimeInterval = ReceiverModel.startupBudget

    @State private var elapsed: TimeInterval = 0
    @State private var didReportTimeout = false

    var body: some View {
        ZStack {
            BattlePalette.background.ignoresSafeArea()

            VStack(spacing: 18) {
                Spacer(minLength: 8)

                BattleSplashView(
                    variant: BattleBrand.variant,
                    markSize: 118,
                    animatesArcs: model.isSceneActive,
                    versionText: "v\(model.appVersion) (\(model.appBuild)) · \(model.receiverVersion)"
                )
                .frame(maxWidth: .infinity)

                progressBar
                    .padding(.horizontal, 44)

                Text(statusLine)
                    .font(BattleFont.body(14))
                    .foregroundStyle(.white.opacity(0.92))
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 32)
                    .frame(minHeight: 40, alignment: .top)
                    .animation(.default, value: statusLine)

                Text(auxiliaryLine)
                    .font(BattleFont.caption(11))
                    .foregroundStyle(BattlePalette.secondaryText)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 32)

                Spacer(minLength: 8)
            }
            .padding(.vertical, 20)
        }
        .onAppear(perform: startCycle)
    }

    // MARK: - Sub views

    private var progressBar: some View {
        GeometryReader { geometry in
            let width = geometry.size.width
            let fraction = min(1, max(0, elapsed / max(timeout, 0.1)))
            ZStack(alignment: .leading) {
                Capsule(style: .continuous)
                    .fill(BattlePalette.chipFill)
                Capsule(style: .continuous)
                    .fill(BattleGradient.radarSweep)
                    .frame(width: max(6, width * fraction))
                    .animation(.linear(duration: 0.25), value: fraction)
            }
        }
        .frame(height: 6)
    }

    // MARK: - Copy

    /// Exact status strings pinned by the interface contract, ordered by phase.
    private var statusLine: String {
        switch model.phase {
        case .idle, .preflight, .unknown:
            return "正在确认联网权限"
        case .starting:
            if model.webPort == 0 {
                return elapsed < timeout * 0.6
                    ? "正在选择可用端口并启动接收器"
                    : "正在启动 SOCKS5 TCP/UDP 接收器"
            }
            return "端口已绑定，正在等待雷达页面"
        case .stopping:
            return "正在停止接收器"
        case .running:
            return "端口已绑定，正在等待雷达页面"
        case .failed:
            return model.receiverFailureMessage ?? "接收器启动失败"
        }
    }

    private var auxiliaryLine: String {
        switch model.phase {
        case .idle, .preflight, .unknown:
            return "首次启动会弹出「本地网络」权限，请允许。"
        case .starting, .running:
            return "端口段 2025–2045 · 数据目录 \(model.dataDirectory)"
        case .failed:
            return "可在错误界面点击「重试」。"
        case .stopping:
            return "正在释放端口与 UDP 中继表。"
        }
    }

    // MARK: - Timing

    private func startCycle() {
        didReportTimeout = false
        elapsed = 0

        Task { @MainActor in
            let step: TimeInterval = 0.25
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: UInt64(step * 1_000_000_000))
                if Task.isCancelled { return }
                elapsed += step
                if elapsed >= timeout, !didReportTimeout {
                    didReportTimeout = true
                    model.handleSplashTimeout()
                    return
                }
            }
        }
    }
}

// MARK: - Animated signal arcs

/// Three expanding arcs that pulse outward - the "listening" metaphor used on
/// the splash screen and in the pairing sheet header.
struct SignalArcsView: View {
    var animating: Bool
    var tint: Color = BattlePalette.amber

    var body: some View {
        ZStack {
            ForEach(0..<3, id: \.self) { index in
                Circle()
                    .strokeBorder(
                        tint.opacity(animating ? 0.14 : 0.42),
                        lineWidth: 1.5
                    )
                    .scaleEffect(animating ? 1.0 : 0.52)
                    .animation(
                        .easeOut(duration: 1.6)
                        .repeatForever(autoreverses: false)
                        .delay(Double(index) * 0.45),
                        value: animating
                    )
            }
            Circle()
                .fill(tint.opacity(0.10))
                .scaleEffect(animating ? 1.02 : 0.7)
                .animation(
                    .easeInOut(duration: 1.6).repeatForever(autoreverses: true),
                    value: animating
                )
        }
        .allowsHitTesting(false)
    }
}
