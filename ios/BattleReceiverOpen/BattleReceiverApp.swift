//
//  BattleReceiverApp.swift
//  BattleReceiverOpen
//
//  SwiftUI lifecycle entry point (the original binary's `main`).
//
//  The app is a LAN radar receiver:
//    1. `battle_proxy_start` (libbattle_proxy.a, C ABI in INTERFACES.md 1) picks
//       the first free port in 2025...2045 and binds SOCKS5 TCP + UDP there
//    2. the local radar page is served on the same port and rendered in
//       WKWebView (`BattleWebView`)
//    3. the pairing sheet tells device B (the one running the game) where to
//       point its Shadowrocket / Hiddify SOCKS5 proxy and UDP relay
//
//  Portrait + landscape, iPhone + iPad, zh-Hans UI, iOS 16 minimum.
//

import SwiftUI

@main
struct BattleReceiverApp: App {

    @StateObject private var model = ReceiverModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootContainerView(model: model)
                .preferredColorScheme(.dark)
                .environment(\.locale, Locale(identifier: "zh-Hans"))
                .onChange(of: scenePhase) { newPhase in
                    // The model decides what a phase change means: resume the
                    // 2 s status poll, re-derive the LAN address, or suspend.
                    model.updateSceneActivity(newPhase)
                }
        }
    }
}

/// Phase driven container: the splash screen owns the boot sequence, the
/// failure view owns recovery, and the root view only appears once a radar URL
/// exists.
private struct RootContainerView: View {

    @ObservedObject var model: ReceiverModel

    var body: some View {
        Group {
            if model.hasBooted, model.browserURL != nil {
                NavigationStack {
                    ReceiverRootView(model: model)
                }
            } else if model.phase == .failed {
                ReceiverFailureView(
                    model: model,
                    onRetry: { model.retry() },
                    onDiagnostics: { model.presentedSheet = .diagnostics }
                )
            } else {
                // `id` restarts the splash timer whenever the boot cycle is
                // re-entered (retry, or returning from a failure).
                ReceiverStartupView(model: model)
                    .id(splashIdentity)
            }
        }
        .onAppear {
            if !model.hasBooted, model.phase != .failed {
                model.start()
            }
        }
        .onChange(of: model.phase) { phase in
            switch phase {
            case .running:
                // A running core without a radar URL is a contract violation;
                // the splash watchdog is the right place to notice it.
                if model.browserURL == nil {
                    model.handleSplashTimeout()
                }
            case .idle:
                if !model.hasBooted {
                    model.start()
                }
            default:
                break
            }
        }
    }

    /// Cheap identity that changes once per failure so the splash timer resets.
    private var splashIdentity: String {
        "\(model.phase.rawValue)-\(model.receiverFailureMessage ?? "ok")"
    }
}
