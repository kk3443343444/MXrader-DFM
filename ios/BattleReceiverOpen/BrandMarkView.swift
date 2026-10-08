//
//  BrandMarkView.swift
//  BattleReceiverOpen
//
//  Brand mark for the "MXrader 三角洲" build. The mark prefers the operator
//  supplied bitmap (BattleMark.png / MXMark.png dropped into the target) and
//  falls back to a vector drawing so the splash and the navigation bar always
//  render something meaningful, even with an empty asset catalog.
//
//  The active variant is read from Info.plist (`BattleBrandVariant = mx`).
//

import SwiftUI

// MARK: - Variant

enum BattleBrandVariant: String, CaseIterable, Identifiable {
    /// "MXrader 三角洲" build - the only variant this target ships.
    case mx
    /// Neutral build without the MX lettering (kept for compatibility with the
    /// original binary's second brand slot).
    case battle
    case unknown

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .mx: return "MXrader 三角洲"
        case .battle: return "Battle Receiver"
        case .unknown: return "Battle Receiver"
        }
    }

    /// Short two letter glyph used by the vector fallback.
    var glyph: String {
        switch self {
        case .mx: return "MX"
        case .battle: return "BR"
        case .unknown: return "BR"
        }
    }
}

enum BattleBrand {
    /// `BattleBrandVariant` from Info.plist, defaulting to `mx`.
    static var current: String {
        let raw = (Bundle.main.object(forInfoDictionaryKey: "BattleBrandVariant") as? String) ?? "mx"
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return trimmed.isEmpty ? "mx" : trimmed
    }

    static var variant: BattleBrandVariant {
        BattleBrandVariant(rawValue: current) ?? .battle
    }

    /// Radar page query parameter (`battle.html?brand=mx`).
    static var queryValue: String { current }
}

// MARK: - Mark view

/// Draws the brand mark. `variant` defaults to the Info.plist value, but the
/// task contract pins it to "mx" so callers may pass it explicitly.
struct BrandMarkView: View {
    var variant: BattleBrandVariant = BattleBrand.variant
    var size: CGFloat = 96
    /// Draws the mark inside a rounded tile (splash style) or bare (toolbar).
    var showsTile: Bool = true

    private let bitmapCandidates = ["MXMark", "BattleMark", "MXMark@3x", "BattleMark@3x"]

    var body: some View {
        Group {
            if let image = bitmap {
                Image(uiImage: image)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .clipShape(RoundedRectangle(cornerRadius: size * 0.22, style: .continuous))
            } else {
                VectorBrandMark(variant: variant, showsTile: showsTile)
            }
        }
        .frame(width: size, height: size)
        .accessibilityLabel(Text("MXrader 三角洲 标识"))
    }

    /// First existing bitmap in the bundle: `BattleMark.png` / `MXMark.png`.
    private var bitmap: UIImage? {
        for name in bitmapCandidates {
            if let image = UIImage(named: name) { return image }
        }
        return nil
    }
}

// MARK: - Vector fallback

/// Pure SwiftUI mark: radar rings, a sweep wedge and the variant glyph.
/// Everything is expressed with `Path` so no assets are required.
struct VectorBrandMark: View {
    var variant: BattleBrandVariant = .mx
    var showsTile: Bool = true

    var body: some View {
        GeometryReader { geometry in
            let side = min(geometry.size.width, geometry.size.height)
            ZStack {
                if showsTile {
                    RoundedRectangle(cornerRadius: side * 0.22, style: .continuous)
                        .fill(BattleGradient.markFill)
                        .overlay(
                            RoundedRectangle(cornerRadius: side * 0.22, style: .continuous)
                                .strokeBorder(BattlePalette.hairline, lineWidth: 1)
                        )
                }

                // Radar rings.
                ForEach(0..<3, id: \.self) { ring in
                    Circle()
                        .strokeBorder(
                            BattlePalette.amber.opacity(0.55 - Double(ring) * 0.13),
                            lineWidth: max(1, side * 0.012)
                        )
                        .frame(width: side * (0.36 + CGFloat(ring) * 0.20),
                               height: side * (0.36 + CGFloat(ring) * 0.20))
                }

                // Sweep wedge.
                SweepWedge()
                    .fill(BattleGradient.radarSweep)
                    .frame(width: side * 0.82, height: side * 0.82)
                    .clipShape(Circle())

                // Crosshair.
                Crosshair()
                    .stroke(BattlePalette.hairline, lineWidth: max(1, side * 0.010))
                    .frame(width: side * 0.80, height: side * 0.80)

                // Blip in the sweep.
                Circle()
                    .fill(BattlePalette.amber)
                    .frame(width: side * 0.055, height: side * 0.055)
                    .offset(x: side * 0.17, y: -side * 0.13)
                    .shadow(color: BattlePalette.amber.opacity(0.8), radius: side * 0.03)

                // Variant glyph, bottom aligned so it does not fight the sweep.
                Text(variant.glyph)
                    .font(.system(size: side * 0.22, weight: .heavy, design: .rounded))
                    .foregroundStyle(.white.opacity(0.92))
                    .offset(y: side * 0.30)
            }
            .frame(width: side, height: side)
        }
        .aspectRatio(1, contentMode: .fit)
    }
}

/// Quarter-circle wedge used as the radar sweep head.
private struct SweepWedge: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        let center = CGPoint(x: rect.midX, y: rect.midY)
        let radius = min(rect.width, rect.height) / 2
        path.move(to: center)
        path.addArc(
            center: center,
            radius: radius,
            startAngle: .degrees(-78),
            endAngle: .degrees(22),
            clockwise: false
        )
        path.closeSubpath()
        return path
    }
}

/// Horizontal + vertical crosshair across the ring area.
private struct Crosshair: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.minX, y: rect.midY))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.midY))
        path.move(to: CGPoint(x: rect.midX, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.midX, y: rect.maxY))
        return path
    }
}
