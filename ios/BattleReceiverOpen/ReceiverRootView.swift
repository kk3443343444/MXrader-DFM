//
//  ReceiverRootView.swift
//  BattleReceiverOpen
//
//  Main screen: the local radar page fills the window, the navigation bar
//  carries the pairing / diagnostics entry points, and every hard failure that
//  the web layer or the core can produce is surfaced as an alert or an inline
//  overlay with an explicit retry path.
//

import SwiftUI

struct ReceiverRootView: View {

    @ObservedObject var model: ReceiverModel

    @State private var failureAlert: FailureAlert?
    @State private var webErrorMessage: String?

    /// Wrapper so `.alert(item:)` can carry the retry semantics of the error.
    struct FailureAlert: Identifiable {
        enum Kind {
            /// WebContent process died / navigation failed: reload only.
            case reload
            /// "HTML 已载入但地图前端未完成挂载": restart the receiver.
            case restart
        }

        let id = UUID()
        let title: String
        let message: String
        let kind: Kind
    }

    var body: some View {
        content
            .battleBackground()
            .navigationTitle("局域网雷达接收器")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { toolbarContent }
            .sheet(item: $model.presentedSheet) { sheet in
                switch sheet {
                case .pairing:
                    PairingSheet(model: model)
                        .presentationDetents([.medium, .large])
                case .diagnostics:
                    DiagnosticsSheet(model: model)
                        .presentationDetents([.medium, .large])
                }
            }
            .alert(item: $failureAlert) { alert in
                switch alert.kind {
                case .reload:
                    return Alert(
                        title: Text(alert.title),
                        message: Text(alert.message),
                        primaryButton: .default(Text("重试")) {
                            webErrorMessage = nil
                            model.reloadWebView()
                        },
                        secondaryButton: .cancel(Text("好"))
                    )
                case .restart:
                    return Alert(
                        title: Text(alert.title),
                        message: Text(alert.message),
                        primaryButton: .default(Text("重试")) {
                            webErrorMessage = nil
                            model.retry()
                        },
                        secondaryButton: .cancel(Text("好"))
                    )
                }
            }
    }

    // MARK: - Body

    @ViewBuilder
    private var content: some View {
        if let url = model.browserURL {
            ZStack {
                BattleWebView(
                    url: url,
                    reloadToken: model.webReloadToken,
                    logNavigationToken: model.battleLogToken,
                    onReady: { webErrorMessage = nil },
                    onLoadFailure: handleWebFailure
                )
                .ignoresSafeArea(edges: .bottom)

                if let webErrorMessage {
                    inlineErrorOverlay(message: webErrorMessage)
                }
            }
        } else {
            waitingPlaceholder
        }
    }

    private var waitingPlaceholder: some View {
        VStack(spacing: 14) {
            ProgressView()
                .progressViewStyle(.circular)
                .tint(BattlePalette.amber)
            Text("正在等待雷达页面地址")
                .font(BattleFont.body(15))
                .foregroundStyle(.white)
            Text("接收器返回状态后会自动载入 http://127.0.0.1:\(model.webPort == 0 ? 2025 : model.webPort)/battle.html?brand=\(model.brand)")
                .font(BattleFont.mono(11))
                .foregroundStyle(BattlePalette.secondaryText)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 28)
            BattleActionButton(title: "重新启动接收服务", systemImage: "arrow.triangle.2.circlepath") {
                model.retry()
            }
            .padding(.horizontal, 40)
            .padding(.top, 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func inlineErrorOverlay(message: String) -> some View {
        VStack(spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.system(size: 30, weight: .semibold))
                .foregroundStyle(BattlePalette.warning)

            Text("雷达页面异常")
                .font(BattleFont.title(17))
                .foregroundStyle(.white)

            Text(message)
                .font(BattleFont.body(13))
                .foregroundStyle(BattlePalette.secondaryText)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)

            HStack(spacing: 10) {
                BattleActionButton(title: "重新载入页面", systemImage: "arrow.clockwise") {
                    webErrorMessage = nil
                    model.reloadWebView()
                }
                BattleActionButton(title: "重启服务", systemImage: "arrow.triangle.2.circlepath") {
                    webErrorMessage = nil
                    model.retry()
                }
            }
            .padding(.top, 2)
        }
        .padding(20)
        .frame(maxWidth: 420)
        .background(
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .fill(BattlePalette.surface.opacity(0.97))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .strokeBorder(BattlePalette.hairline, lineWidth: 1)
        )
        .padding(24)
        .transition(.opacity)
    }

    // MARK: - Toolbar

    @ToolbarContentBuilder
    private var toolbarContent: some ToolbarContent {
        ToolbarItem(placement: .navigationBarLeading) {
            Menu {
                Button {
                    model.presentedSheet = .pairing
                } label: {
                    Label("配对信息", systemImage: "qrcode")
                }
                Button {
                    model.presentedSheet = .diagnostics
                } label: {
                    Label("诊断信息", systemImage: "stethoscope")
                }
                Divider()
                Button {
                    model.reloadWebView()
                } label: {
                    Label("刷新雷达界面", systemImage: "arrow.clockwise")
                }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .font(.system(size: 17, weight: .semibold))
            }
            .accessibilityLabel(Text("配对与诊断"))
        }

        ToolbarItem(placement: .principal) {
            BattleStatusChip(
                text: chipText,
                color: model.phase == .running ? BattlePalette.success : BattlePalette.amber
            )
        }

        ToolbarItem(placement: .navigationBarTrailing) {
            Button {
                model.reloadWebView()
            } label: {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 16, weight: .semibold))
            }
            .accessibilityLabel(Text("刷新雷达界面"))
        }
    }

    private var chipText: String {
        let port = model.socksPort == 0 ? model.webPort : model.socksPort
        return "\(model.phase.localizedLabel) · \(port == 0 ? "—" : "\(port)")"
    }

    // MARK: - Failure routing

    private func handleWebFailure(_ message: String) {
        webErrorMessage = message
        if message.contains("地图前端未完成挂载") {
            failureAlert = FailureAlert(
                title: "雷达前端未就绪",
                message: message,
                kind: .restart
            )
        } else if message.contains("Web 内容进程已终止") {
            failureAlert = FailureAlert(
                title: "Web 内容进程已终止",
                message: message,
                kind: .reload
            )
        } else {
            failureAlert = FailureAlert(
                title: "雷达页面加载失败",
                message: message,
                kind: .reload
            )
        }
    }
}
