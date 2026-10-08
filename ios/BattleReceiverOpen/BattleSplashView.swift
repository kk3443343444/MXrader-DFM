//
//  BattleSplashView.swift
//  BattleReceiverOpen
//
//  Launch composition: brand mark, app name, animated signal arcs.
//
//  This is the reusable launch *composition* (shared by the boot screen and by
//  `UILaunchScreen`-adjacent surfaces). `ReceiverStartupView` layers the boot
//  progress and the status strings on top of it; previews and the pairing sheet
//  use it standalone.
//

import SwiftUI

struct BattleSplashView: View {

    var variant: BattleBrandVariant = BattleBrand.variant
    var markSize: CGFloat = 120
    /// Set to false to freeze the arcs (used in screenshots / reduce-motion).
    var animatesArcs: Bool = true
    var versionText: String = "v\(Bundle.main.battleMarketingVersion) (\(Bundle.main.battleBuildNumber))"

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var arcsAnimating = false
    @State private var sweepRunning = false

    private var effectiveAnimation: Bool {
        animatesArcs && !reduceMotion
    }

    var body: some View {
        ZStack {
            background

            VStack(spacing: 20) {
                Spacer(minLength: 0)

                ZStack {
                    SignalArcsView(animating: arcsAnimating)
                        .frame(width: markSize * 2.05, height: markSize * 2.05)

                    SweepRing()
                        .fill(BattleGradient.radarSweep)
                        .frame(width: markSize * 1.7, height: markSize * 1.7)
                        .clipShape(Circle())
                        .rotationEffect(.degrees(sweepRunning ? 360 : 0))
                        .animation(
                            effectiveAnimation
                                ? .linear(duration: 6).repeatForever(autoreverses: false)
                                : .default,
                            value: sweepRunning
                        )
                        .opacity(0.55)

                    BrandMarkView(variant: variant, size: markSize)
                }
                .frame(height: markSize * 2.2)

                VStack(spacing: 6) {
                    Text("MXrader 三角洲")
                        .font(BattleFont.title(26))
                        .foregroundStyle(.white)
                    Text(versionText)
                        .font(BattleFont.caption(12))
                        .foregroundStyle(BattlePalette.secondaryText)
                    Text("局域网雷达接收器")
                        .font(BattleFont.caption(12))
                        .foregroundStyle(BattlePalette.amber.opacity(0.9))
                        .padding(.top, 2)
                }

                Spacer(minLength: 0)
            }
            .padding(.horizontal, 24)
        }
        .onAppear(perform: startAnimations)
        .onDisappear {
            arcsAnimating = false
            sweepRunning = false
        }
    }

    // MARK: - Pieces

    private var background: some View {
        ZStack {
            BattlePalette.background.ignoresSafeArea()
            RadialGradient(
                colors: [BattlePalette.amber.opacity(0.10), .clear],
                center: .center,
                startRadius: 8,
                endRadius: markSize * 3.2
            )
            .ignoresSafeArea()
        }
    }

    private func startAnimations() {
        guard effectiveAnimation else {
            arcsAnimating = false
            sweepRunning = false
            return
        }
        withAnimation(.easeInOut(duration: 1.8).repeatForever(autoreverses: true)) {
            arcsAnimating = true
        }
        sweepRunning = true
    }
}

/// Quarter ring used as the rotating radar sweep on the splash.
private struct SweepRing: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        let center = CGPoint(x: rect.midX, y: rect.midY)
        let radius = min(rect.width, rect.height) / 2
        path.move(to: center)
        path.addArc(
            center: center,
            radius: radius,
            startAngle: .degrees(0),
            endAngle: .degrees(65),
            clockwise: false
        )
        path.closeSubpath()
        return path
    }
}
