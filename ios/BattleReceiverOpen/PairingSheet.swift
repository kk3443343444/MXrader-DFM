//
//  PairingSheet.swift
//  BattleReceiverOpen
//
//  Device-B pairing panel. Everything the phone that runs the game needs in
//  order to point its SOCKS5 proxy + UDP relay at this device:
//
//    * the SOCKS5 control address      socks5://<lan-ip>:<port>
//    * the LAN radar page              http://<lan-ip>:<port>/battle.html?brand=mx
//    * the Hiddify / sing-box profile  http://<lan-ip>:<port>/api/socks5/hiddify.json
//      rendered as a scannable QR code (CoreImage qrCodeGenerator)
//    * a short checklist for the iOS client configuration
//
//  The QR payload is the profile URL, so scanning it hands the whole outbound
//  configuration to device B without any typing.
//

import SwiftUI
import CoreImage
import CoreImage.CIFilterBuiltins
import UIKit

struct PairingSheet: View {

    @ObservedObject var model: ReceiverModel
    @State private var refreshing = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: BattleMetrics.sectionSpacing) {
                    header
                    addressSection
                    qrSection
                    checklistSection
                    footnoteSection
                }
                .padding(18)
            }
            .background(BattlePalette.background.ignoresSafeArea())
            .navigationTitle("配对信息")
            .navigationBarTitleDisplayMode(.inline)
        }
    }

    // MARK: - Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 14) {
                ZStack {
                    SignalArcsView(animating: true, tint: BattlePalette.amber)
                        .frame(width: 68, height: 68)
                    BrandMarkView(variant: BattleBrand.variant, size: 44, showsTile: true)
                }

                VStack(alignment: .leading, spacing: 4) {
                    Text("B 机配对")
                        .font(BattleFont.title(17))
                        .foregroundStyle(.white)
                    Text("在运行游戏的设备上把 SOCKS5 与 UDP 指向本机")
                        .font(BattleFont.caption(11))
                        .foregroundStyle(BattlePalette.secondaryText)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 0)
            }

            HStack(spacing: 8) {
                BattleStatusChip(
                    text: model.phase.localizedLabel,
                    color: model.phase == .running ? BattlePalette.success : BattlePalette.amber
                )
                BattleStatusChip(text: "端口 \(primaryPortText)", color: BattlePalette.amber)
                BattleStatusChip(text: model.brand, color: BattlePalette.secondaryText)
            }
        }
        .battleCard()
    }

    // MARK: - Addresses

    private var addressSection: some View {
        VStack(alignment: .leading, spacing: 14) {
            BattleSectionHeader(
                title: "连接地址",
                subtitle: "本机局域网地址 \(model.displayAddress)"
            )

            CopyRow(label: "SOCKS5 地址", value: model.socksURL, buttonTitle: "复制 SOCKS5 地址")
            CopyRow(label: "局域网雷达网址", value: model.radarDisplayURL, buttonTitle: "复制局域网雷达网址")
            CopyRow(label: "Hiddify 配置", value: model.hiddifyProfileURL, buttonTitle: "复制 Hiddify 配置网址")

            BattleActionButton(
                title: refreshing ? "刷新中…" : "刷新局域网地址",
                systemImage: "wifi"
            ) {
                refresh()
            }
        }
        .battleCard()
    }

    // MARK: - QR code

    private var qrSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            BattleSectionHeader(
                title: "小火箭配置二维码",
                subtitle: "Hiddify / sing-box 分享配置 · 已启用 UDP"
            )

            HStack(alignment: .top, spacing: 14) {
                QRCodeView(text: model.hiddifyProfileURL, side: 148)

                VStack(alignment: .leading, spacing: 8) {
                    Text("扫描后导入出站配置：")
                        .font(BattleFont.caption())
                        .foregroundStyle(BattlePalette.secondaryText)
                    BulletLine(text: "出站协议 SOCKS5，指向本机 \(model.displayAddress)")
                    BulletLine(text: "已开启 UDP 转发（对应 udp_associate）")
                    BulletLine(text: "每源端口独立 NAT，双向并发")
                    Spacer(minLength: 0)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            Text(model.hiddifyProfileURL)
                .font(BattleFont.mono(11))
                .foregroundStyle(BattlePalette.secondaryText)
                .textSelection(.enabled)
                .lineLimit(2)
        }
        .battleCard()
    }

    // MARK: - Checklist

    private var checklistSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            BattleSectionHeader(title: "B 机配置检查清单", subtitle: "逐条确认后再进入游戏")

            CheckLine(text: "小火箭必须启用 UDP 转发")
            CheckLine(text: "必须开启（多流）")
            CheckLine(text: "全局 / TUN（服务器直连）")
            CheckLine(text: "每源端口独立 NAT · 双向并发")
            CheckLine(text: "请确认 B 机已连接 Wi-Fi，并刷新地址。")
            CheckLine(text: "请让本机保持前台并连接电源。启动完成后会显示小火箭配置二维码。")
        }
        .battleCard()
    }

    // MARK: - Footnote

    private var footnoteSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            BattleInfoRow(label: "本机雷达（只读）", value: model.radarDisplayURL)
            BattleInfoRow(label: "总会话", value: "\(model.statusSnapshot.totalSessions)")
            BattleInfoRow(label: "当前会话", value: "\(model.openHandle)")
            if let checked = model.lastHealthCheck {
                BattleInfoRow(label: "最近心跳", value: Self.timeFormatter.string(from: checked))
            }
            Text("普通 SOCKS5 与局域网雷达网页均不提供传输加密，只应在可信局域网中使用。")
                .font(BattleFont.caption(11))
                .foregroundStyle(BattlePalette.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        .battleCard()
    }

    // MARK: - Helpers

    private var primaryPortText: String {
        let port = model.socksPort == 0 ? model.webPort : model.socksPort
        return port == 0 ? "—" : "\(port)"
    }

    private static let timeFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "zh_Hans_CN")
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()

    private func refresh() {
        guard !refreshing else { return }
        refreshing = true
        Task { @MainActor in
            await model.refreshEndpoint()
            model.refreshStatusNow()
            try? await Task.sleep(nanoseconds: 300_000_000)
            refreshing = false
        }
    }
}

// MARK: - Copyable address row

/// Address block with its own copy button and a self-resetting "已复制" state.
private struct CopyRow: View {
    let label: String
    let value: String
    let buttonTitle: String

    @State private var copied = false
    @State private var resetTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(label)
                .font(BattleFont.caption())
                .foregroundStyle(BattlePalette.secondaryText)

            Text(value)
                .font(BattleFont.mono(13))
                .foregroundStyle(.white)
                .textSelection(.enabled)
                .lineLimit(2)
                .minimumScaleFactor(0.75)
                .padding(9)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .fill(BattlePalette.codeBackground)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .strokeBorder(BattlePalette.hairline, lineWidth: 1)
                )

            Button(action: copyAction) {
                HStack(spacing: 6) {
                    Image(systemName: copied ? "checkmark.circle.fill" : "doc.on.doc")
                        .font(.system(size: 13, weight: .semibold))
                    Text(copied ? "已复制" : buttonTitle)
                        .font(BattleFont.caption(13))
                }
                .foregroundStyle(copied ? BattlePalette.success : BattlePalette.amber)
            }
            .buttonStyle(.plain)
        }
        .onDisappear { resetTask?.cancel() }
    }

    private func copyAction() {
        UIPasteboard.general.string = value
        copied = true
        resetTask?.cancel()
        resetTask = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard !Task.isCancelled else { return }
            copied = false
        }
    }
}

// MARK: - Small pieces

private struct BulletLine: View {
    let text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text("·")
                .font(BattleFont.headline(14))
                .foregroundStyle(BattlePalette.amber)
            Text(text)
                .font(BattleFont.caption(12))
                .foregroundStyle(.white.opacity(0.9))
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct CheckLine: View {
    let text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "checkmark.square.fill")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(BattlePalette.amber)
            Text(text)
                .font(BattleFont.body(13))
                .foregroundStyle(.white)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

// MARK: - QR code rendering

/// Renders `text` as a QR code with `CIFilter.qrCodeGenerator` and draws the
/// resulting `UIImage` through `Image(uiImage:)`.
struct QRCodeView: View {
    let text: String
    var side: CGFloat = 148

    var body: some View {
        Group {
            if let image = Self.makeQRCode(from: text) {
                Image(uiImage: image)
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
                    .frame(width: side, height: side)
                    .padding(8)
                    .background(
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .fill(Color.white)
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .strokeBorder(BattlePalette.hairline, lineWidth: 1)
                    )
                    .accessibilityLabel(Text("小火箭配置二维码"))
            } else {
                VStack(spacing: 6) {
                    Image(systemName: "qrcode")
                        .font(.system(size: 30, weight: .regular))
                    Text("二维码生成失败")
                        .font(BattleFont.caption(11))
                }
                .foregroundStyle(BattlePalette.secondaryText)
                .frame(width: side, height: side)
                .background(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .fill(BattlePalette.chipFill)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .strokeBorder(BattlePalette.hairline, lineWidth: 1)
                )
            }
        }
    }

    /// Shared CIContext: one context for every render, the pipeline itself is
    /// cheap but not free.
    private static let context = CIContext(options: [.useSoftwareRenderer: false])

    static func makeQRCode(from text: String, scale: CGFloat = 10) -> UIImage? {
        guard !text.isEmpty, let payload = text.data(using: .utf8) else { return nil }

        let filter = CIFilter.qrCodeGenerator()
        filter.message = payload
        // H (30%) keeps the code readable when scanned off a glossy screen.
        filter.correctionLevel = "H"

        guard let output = filter.outputImage else { return nil }
        let scaled = output.transformed(by: CGAffineTransform(scaleX: scale, y: scale))

        guard let cgImage = context.createCGImage(scaled, from: scaled.extent) else { return nil }
        return UIImage(cgImage: cgImage)
    }
}
