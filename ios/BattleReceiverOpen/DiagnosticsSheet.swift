//
//  DiagnosticsSheet.swift
//  BattleReceiverOpen
//
//  Read-only diagnostics panel:
//    * the full status JSON (pretty printed, exactly what the core returned)
//    * the counters a field engineer asks about first
//    * three actions: reload the radar page, restart the receiver service and
//      jump to the radar network log view
//
//  The admin routes behind these buttons are local-only by contract
//  (INTERFACES.md 4: /api/admin/* requires loopback + BATTLE_ADMIN_TOKEN), and
//  the warning at the bottom spells that out for the user.
//

import SwiftUI
import UIKit

struct DiagnosticsSheet: View {

    @ObservedObject var model: ReceiverModel

    @State private var busy: String?
    @State private var lastActionResult: String?
    @State private var copiedJSON = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: BattleMetrics.sectionSpacing) {
                    summarySection
                    countersSection
                    actionsSection
                    jsonSection
                    warningSection
                }
                .padding(18)
            }
            .background(BattlePalette.background.ignoresSafeArea())
            .navigationTitle("诊断信息")
            .navigationBarTitleDisplayMode(.inline)
        }
    }

    // MARK: - Summary

    private var summarySection: some View {
        VStack(alignment: .leading, spacing: 10) {
            BattleSectionHeader(
                title: "运行摘要",
                subtitle: "battle_proxy_status() · \(model.receiverVersion)"
            )

            BattleInfoRow(label: "阶段", value: model.phase.localizedLabel, mono: false)
            BattleInfoRow(label: "模式", value: model.statusSnapshot.mode ?? "ios_receiver")
            BattleInfoRow(label: "主端口", value: portText)
            BattleInfoRow(label: "SOCKS5", value: model.socksURL)
            BattleInfoRow(label: "雷达网址", value: model.radarDisplayURL)
            BattleInfoRow(label: "已完成会话", value: "\(model.statusSnapshot.totalSessions)")
            BattleInfoRow(label: "已授权", value: model.runtimeStatus.authorized ? "是" : "否")
            if let tail = model.runtimeStatus.cardTail, !tail.isEmpty {
                BattleInfoRow(label: "卡密尾号", value: tail)
            }
            if let checked = model.lastHealthCheck {
                BattleInfoRow(label: "最近心跳", value: Self.timeFormatter.string(from: checked))
            }
            if let error = model.lastStatusError, !error.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("最近错误")
                        .font(BattleFont.caption())
                        .foregroundStyle(BattlePalette.warning)
                    Text(error)
                        .font(BattleFont.mono(11))
                        .foregroundStyle(.white)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .battleCard()
    }

    // MARK: - Counters

    private var countersSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            BattleSectionHeader(title: "计数器", subtitle: "IPC 计数器快照（每次 2 秒轮询刷新）")

            LazyVGrid(
                columns: [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)],
                spacing: 10
            ) {
                CounterTile(title: "active_sessions", value: model.statusSnapshot.activeSessions, tint: BattlePalette.amber)
                CounterTile(title: "total_sessions", value: model.statusSnapshot.totalSessions, tint: BattlePalette.success)
                CounterTile(title: "udp_packets_up", value: model.statusSnapshot.udpPacketsUp, tint: BattlePalette.amber)
                CounterTile(title: "udp_packets_down", value: model.statusSnapshot.udpPacketsDown, tint: BattlePalette.success)
                CounterTile(title: "udp_invalid_packets", value: model.statusSnapshot.udpInvalidPackets, tint: BattlePalette.warning)
                CounterTile(title: "tcp_relay_failures", value: model.statusSnapshot.tcpRelayFailures, tint: BattlePalette.warning)
                CounterTile(title: "loot_payloads_skipped", value: model.statusSnapshot.lootPayloadsSkipped, tint: BattlePalette.secondaryText)
                CounterTile(title: "open_handle", value: model.openHandle, tint: BattlePalette.amber)
            }
        }
        .battleCard()
    }

    // MARK: - Actions

    private var actionsSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            BattleSectionHeader(title: "操作", subtitle: "全部仅在本机生效")

            Button {
                model.reloadWebView()
                lastActionResult = "已发送刷新指令"
            } label: {
                actionLabel(title: "刷新雷达界面", systemImage: "arrow.clockwise", tint: BattlePalette.amber)
            }
            .buttonStyle(.plain)

            Button {
                restart()
            } label: {
                actionLabel(
                    title: busy == "restart" ? "正在重启…" : "重新启动接收服务",
                    systemImage: "arrow.triangle.2.circlepath",
                    tint: BattlePalette.amber
                )
            }
            .buttonStyle(.plain)
            .disabled(busy != nil)

            Button {
                openNetworkLog()
            } label: {
                actionLabel(
                    title: "雷达设置 → 网络日志",
                    systemImage: "doc.text.magnifyingglass",
                    tint: BattlePalette.amber
                )
            }
            .buttonStyle(.plain)

            if let lastActionResult {
                Text(lastActionResult)
                    .font(BattleFont.caption(11))
                    .foregroundStyle(BattlePalette.secondaryText)
            }
        }
        .battleCard()
    }

    private func actionLabel(title: String, systemImage: String, tint: Color) -> some View {
        HStack(spacing: 8) {
            Image(systemName: systemImage)
                .font(.system(size: 14, weight: .semibold))
            Text(title)
                .font(BattleFont.headline(14))
            Spacer(minLength: 0)
        }
        .padding(.vertical, 11)
        .padding(.horizontal, 12)
        .frame(maxWidth: .infinity)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(BattlePalette.chipFill)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(tint.opacity(0.55), lineWidth: 1)
        )
        .foregroundStyle(tint)
    }

    // MARK: - Raw JSON

    private var jsonSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                BattleSectionHeader(title: "完整状态 JSON", subtitle: "battle_proxy_status() 原样输出")
                Spacer(minLength: 8)
                Button {
                    UIPasteboard.general.string = statusJSON
                    copiedJSON = true
                    Task { @MainActor in
                        try? await Task.sleep(nanoseconds: 1_500_000_000)
                        copiedJSON = false
                    }
                } label: {
                    Image(systemName: copiedJSON ? "checkmark.circle.fill" : "doc.on.doc")
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(copiedJSON ? BattlePalette.success : BattlePalette.amber)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(Text("复制状态 JSON"))

                Button {
                    model.refreshStatusNow()
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(BattlePalette.amber)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(Text("刷新状态 JSON"))
            }

            ScrollView([.horizontal, .vertical], showsIndicators: true) {
                Text(statusJSON)
                    .font(BattleFont.mono(11))
                    .foregroundStyle(.white.opacity(0.92))
                    .textSelection(.enabled)
                    .fixedSize(horizontal: true, vertical: true)
                    .padding(10)
            }
            .frame(maxWidth: .infinity, minHeight: 190, alignment: .topLeading)
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

    // MARK: - Warning

    private var warningSection: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "lock.shield")
                .font(.system(size: 16, weight: .semibold))
                .foregroundStyle(BattlePalette.warning)
            Text("普通 SOCKS5 与局域网雷达网页均不提供传输加密，只应在可信局域网中使用。远程雷达页面为只读，重置、调试和解析开关仅允许本机操作。")
                .font(BattleFont.caption(12))
                .foregroundStyle(BattlePalette.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        .battleCard()
    }

    // MARK: - Derived

    private var portText: String {
        let port = model.statusSnapshot.primaryPort ?? model.statusSnapshot.socksPort ?? model.statusSnapshot.webPort ?? 0
        return port == 0 ? "—" : "\(port)"
    }

    private var statusJSON: String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        guard let data = try? encoder.encode(model.statusSnapshot),
              let object = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed]),
              let pretty = try? JSONSerialization.data(
                  withJSONObject: object,
                  options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
              ),
              let text = String(data: pretty, encoding: .utf8)
        else {
            return "{}"
        }
        return text
    }

    private static let timeFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "zh_Hans_CN")
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()

    // MARK: - Actions

    private func restart() {
        guard busy == nil else { return }
        busy = "restart"
        Task { @MainActor in
            await model.restartReceiver()
            busy = nil
            lastActionResult = "已请求重启接收服务"
        }
    }

    /// "雷达设置 → 网络日志": asks the local radar page to open its log panel.
    private func openNetworkLog() {
        model.openNetworkLog()
        lastActionResult = "已请求雷达设置 → 网络日志"
    }
}

// MARK: - Counter tile

private struct CounterTile: View {
    let title: String
    let value: Int
    var tint: Color = BattlePalette.amber

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(BattleFont.mono(10))
                .foregroundStyle(BattlePalette.secondaryText)
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            Text("\(value)")
                .font(.system(size: 19, weight: .semibold, design: .monospaced))
                .foregroundStyle(tint)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(BattlePalette.chipFill)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(BattlePalette.hairline, lineWidth: 1)
        )
    }
}
