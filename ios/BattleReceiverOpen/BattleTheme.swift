//
//  BattleTheme.swift
//  BattleReceiverOpen
//
//  Central design tokens for the shell: colors, fonts, gradients and the
//  reusable `battleCard()` container used by every panel. Keeping the tokens in
//  one file is what lets the Chinese copy stay identical across screens.
//

import SwiftUI

// MARK: - Palette

enum BattlePalette {

    /// Panel background sitting on top of `background`.
    static let surface = Color(red: 0.086, green: 0.102, blue: 0.129)
    /// Window background.
    static let background = Color(red: 0.039, green: 0.047, blue: 0.063)
    /// 1 px separator / card stroke.
    static let hairline = Color(red: 0.196, green: 0.224, blue: 0.271)
    /// Primary accent (radar sweep amber).
    static let amber = Color(red: 0.941, green: 0.678, blue: 0.235)
    /// Warning / failure accent.
    static let warning = Color(red: 0.898, green: 0.345, blue: 0.290)
    /// Secondary copy.
    static let secondaryText = Color(red: 0.561, green: 0.612, blue: 0.678)
    /// Support colors reused by the mark and status chips.
    static let success = Color(red: 0.259, green: 0.741, blue: 0.541)
    static let chipFill = Color(red: 0.129, green: 0.153, blue: 0.192)
    static let codeBackground = Color(red: 0.055, green: 0.067, blue: 0.086)
}

// MARK: - Metrics and fonts

enum BattleMetrics {
    static let cardCornerRadius: CGFloat = 14
    static let cardPadding: CGFloat = 14
    static let sectionSpacing: CGFloat = 16
    static let rowSpacing: CGFloat = 10
}

enum BattleFont {
    static func title(_ size: CGFloat = 20) -> Font {
        .system(size: size, weight: .bold, design: .rounded)
    }

    static func headline(_ size: CGFloat = 16) -> Font {
        .system(size: size, weight: .semibold, design: .rounded)
    }

    static func body(_ size: CGFloat = 14) -> Font {
        .system(size: size, weight: .regular, design: .rounded)
    }

    static func caption(_ size: CGFloat = 12) -> Font {
        .system(size: size, weight: .medium, design: .rounded)
    }

    /// Monospaced face for URLs, ports and the raw status JSON.
    static func mono(_ size: CGFloat = 13) -> Font {
        .system(size: size, weight: .regular, design: .monospaced)
    }
}

// MARK: - Gradients

enum BattleGradient {
    /// Radar sweep: amber head fading into the panel color.
    static let radarSweep = LinearGradient(
        colors: [BattlePalette.amber.opacity(0.95), BattlePalette.amber.opacity(0.05)],
        startPoint: .topLeading,
        endPoint: .bottomTrailing
    )

    static let markFill = LinearGradient(
        colors: [Color(red: 0.180, green: 0.208, blue: 0.259), Color(red: 0.078, green: 0.094, blue: 0.122)],
        startPoint: .topLeading,
        endPoint: .bottomTrailing
    )

    static let failureFill = LinearGradient(
        colors: [BattlePalette.warning.opacity(0.22), BattlePalette.surface],
        startPoint: .top,
        endPoint: .bottom
    )
}

// MARK: - Card container

/// Rounded 14 pt surface with a hairline stroke and standard padding.
struct BattleCardModifier: ViewModifier {
    var padding: CGFloat = BattleMetrics.cardPadding
    var cornerRadius: CGFloat = BattleMetrics.cardCornerRadius

    func body(content: Content) -> some View {
        content
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .fill(BattlePalette.surface)
            )
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .strokeBorder(BattlePalette.hairline, lineWidth: 1)
            )
    }
}

extension View {
    /// Reusable panel: rounded 14, hairline stroke, standard padding.
    func battleCard(padding: CGFloat = BattleMetrics.cardPadding) -> some View {
        modifier(BattleCardModifier(padding: padding))
    }

    /// Full-screen themed background applied at the container level.
    func battleBackground() -> some View {
        background(BattlePalette.background.ignoresSafeArea())
    }
}

// MARK: - Small shared controls

/// Section header used inside sheets.
struct BattleSectionHeader: View {
    let title: String
    var subtitle: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
                .font(BattleFont.headline())
                .foregroundStyle(BattlePalette.amber)
            if let subtitle {
                Text(subtitle)
                    .font(BattleFont.caption())
                    .foregroundStyle(BattlePalette.secondaryText)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// `label` / `value` row with a monospaced, selectable value.
struct BattleInfoRow: View {
    let label: String
    let value: String
    var mono: Bool = true
    var tint: Color = BattlePalette.secondaryText

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(label)
                .font(BattleFont.caption())
                .foregroundStyle(tint)
            Spacer(minLength: 8)
            Text(value)
                .font(mono ? BattleFont.mono(13) : BattleFont.body(14))
                .foregroundStyle(.white)
                .multilineTextAlignment(.trailing)
                .textSelection(.enabled)
        }
    }
}

/// Status chip: small pill with a leading dot.
struct BattleStatusChip: View {
    let text: String
    let color: Color

    var body: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(color)
                .frame(width: 7, height: 7)
            Text(text)
                .font(BattleFont.caption(11))
                .foregroundStyle(.white)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 5)
        .background(
            Capsule(style: .continuous)
                .fill(BattlePalette.chipFill)
        )
        .overlay(
            Capsule(style: .continuous)
                .strokeBorder(BattlePalette.hairline, lineWidth: 1)
        )
    }
}

/// Primary action button styled for the dark surface.
struct BattleActionButton: View {
    let title: String
    var systemImage: String?
    var tint: Color = BattlePalette.amber
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                if let systemImage {
                    Image(systemName: systemImage)
                        .font(.system(size: 13, weight: .semibold))
                }
                Text(title)
                    .font(BattleFont.headline(14))
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 10)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(BattlePalette.chipFill)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(tint.opacity(0.6), lineWidth: 1)
            )
            .foregroundStyle(tint)
        }
        .buttonStyle(.plain)
    }
}
