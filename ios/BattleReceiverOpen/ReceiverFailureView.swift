//
//  ReceiverFailureView.swift
//  BattleReceiverOpen
//
//  Terminal error surface for the receiver. Shown when `battle_proxy_start`
//  answers with phase=failed, when the 5 s splash budget expires, or when the
//  web layer reports that the radar page never mounted.
//
//  The primary copy is the port-exhaustion explanation (the only failure that a
//  user can actually act on), and the untouched error string from the core is
//  always visible underneath it so support can read it back verbatim.
//

import SwiftUI
import UIKit

struct ReceiverFailureView: View {

    @ObservedObject var model: ReceiverModel
    /// Overrides the message when the shell (not the core) detected the failure.
    var message: String?
    var onRetry: () -> Void
    var onDiagnostics: (() -> Void)?

    @State private var copied = false

    private var headline: String {
        message ?? model.receiverFailureMessage ?? "接收器启动失败"
    }

    /// Raw string from `battle_proxy_start` / the WebKit error, never rewritten.
    private var rawError: String {
        if let raw = model.lastStatusError, !raw.isEmpty { return raw }
        if let raw = model.receiverFailureMessage, !raw.isEmpty { return raw }
        return "battle_proxy_start 未返回可用状态（phase=failed，error=null）"
    }

    var body: some View {
        ZStack {
            BattlePalette.background.ignoresSafeArea()

            ScrollView {
                VStack(spacing: BattleMetrics.sectionSpacing) {
                    header

                    VStack(alignment: .leading, spacing: 12) {
                        BattleSectionHeader(title: "端口占用说明", subtitle: "端口段 2025–2045 全部尝试完毕")

                        Text("接收端口被其他应用占用（已自动尝试 2025–2045）…")
                            .font(BattleFont.body(14))
                            .foregroundStyle(.white)

                        Text("请关闭正在使用 2025–2045 的其他接收器或调试工具后重试；若手机开了 VPN（小火箭 / Hiddify），请先断开再启动本机接收器。")
                            .font(BattleFont.caption())
                            .foregroundStyle(BattlePalette.secondaryText)
                            .fixedSize(horizontal: false, vertical: true)

                        BattleInfoRow(label: "已尝试端口", value: "2025 – 2045")
                        BattleInfoRow(label: "当前阶段", value: model.phase.localizedLabel, mono: false)
                        BattleInfoRow(label: "数据目录", value: model.dataDirectory)
                    }
                    .battleCard()

                    rawErrorCard

                    VStack(spacing: 10) {
                        Button(action: onRetry) {
                            HStack(spacing: 8) {
                                Image(systemName: "arrow.clockwise")
                                    .font(.system(size: 15, weight: .bold))
                                Text("重试")
                                    .font(BattleFont.title(16))
                            }
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 13)
                            .background(
                                RoundedRectangle(cornerRadius: 12, style: .continuous)
                                    .fill(BattlePalette.amber)
                            )
                            .foregroundStyle(Color.black.opacity(0.9))
                        }
                        .buttonStyle(.plain)

                        if let onDiagnostics {
                            BattleActionButton(title: "诊断信息", systemImage: "stethoscope", action: onDiagnostics)
                        }

                        Button(action: copyRawError) {
                            HStack(spacing: 6) {
                                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                                    .font(.system(size: 13, weight: .semibold))
                                Text(copied ? "已复制原始错误" : "复制原始错误")
                                    .font(BattleFont.caption(13))
                            }
                            .foregroundStyle(BattlePalette.secondaryText)
                        }
                        .buttonStyle(.plain)
                    }
                }
                .padding(20)
            }
        }
    }

    // MARK: - Pieces

    private var header: some View {
        VStack(spacing: 12) {
            ZStack {
                Circle()
                    .fill(BattlePalette.warning.opacity(0.16))
                    .frame(width: 84, height: 84)
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 34, weight: .semibold))
                    .foregroundStyle(BattlePalette.warning)
            }

            Text(headline)
                .font(BattleFont.title(18))
                .foregroundStyle(.white)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)

            Text("接收器没有启动成功，雷达页面与配对信息暂不可用。")
                .font(BattleFont.caption())
                .foregroundStyle(BattlePalette.secondaryText)
                .multilineTextAlignment(.center)
        }
        .padding(.top, 24)
    }

    private var rawErrorCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            BattleSectionHeader(title: "原始错误", subtitle: "battle_proxy_start / WKWebView 原样返回")
            ScrollView(.horizontal, showsIndicators: true) {
                Text(rawError)
                    .font(BattleFont.mono(12))
                    .foregroundStyle(BattlePalette.warning)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: true, vertical: false)
                    .padding(10)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(BattlePalette.codeBackground)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(BattlePalette.hairline, lineWidth: 1)
            )
        }
        .battleCard()
    }

    private func copyRawError() {
        UIPasteboard.general.string = rawError
        copied = true
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            copied = false
        }
    }
}
