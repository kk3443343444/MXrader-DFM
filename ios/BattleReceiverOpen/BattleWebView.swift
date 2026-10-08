//
//  BattleWebView.swift
//  BattleReceiverOpen
//
//  WKWebView host for the local radar page served by the Rust core on
//  `http://127.0.0.1:<port>/battle.html?brand=mx`.
//
//  Responsibilities:
//    * inject a viewport WKUserScript once, before the document body exists
//    * probe for radar readiness (INTERFACES.md 6): dataset.battleReady === '1',
//      `#app` has children and `.leaflet-container` exists
//    * keep the radar alive: reload on demand, tolerate a terminated WebContent
//      process, surface JS alert()/confirm() as native UIAlertController
//    * report "HTML loaded but the map never mounted" as a distinct failure the
//      model can turn into a retry
//
//  Only WebKit + UIKit are used.
//

import SwiftUI
import WebKit

struct BattleWebView: UIViewRepresentable {

    /// Radar page to load; nil keeps the representable inert.
    let url: URL?

    /// Bumped by ReceiverModel.reloadWebView(); any change triggers `reload()`.
    let reloadToken: Int

    /// Bumped by ReceiverModel.openNetworkLog(); any change runs the in-page
    /// navigation request for the radar's network log view.
    var logNavigationToken: Int = 0

    /// Called once when the readiness probe confirmed the Leaflet map mounted.
    var onReady: () -> Void = {}

    /// Called with a human readable Chinese reason when loading or mounting failed.
    var onLoadFailure: (String) -> Void = { _ in }

    /// Probe cadence: 0.5 s per attempt, ~20 attempts (10 s) before giving up.
    static let probeInterval: TimeInterval = 0.5
    static let probeAttemptLimit = 20

    func makeCoordinator() -> Coordinator {
        Coordinator(
            reloadToken: reloadToken,
            logNavigationToken: logNavigationToken,
            onReady: onReady,
            onLoadFailure: onLoadFailure,
            probeInterval: Self.probeInterval,
            probeAttemptLimit: Self.probeAttemptLimit
        )
    }

    func makeUIView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()

        // Radar viewport contract (INTERFACES.md 6): the page is a full-bleed
        // touch canvas, so a fixed viewport script is injected before any
        // document script runs.
        let viewportScript = WKUserScript(
            source: """
            (function () {
              var meta = document.querySelector('meta[name="viewport"]');
              if (!meta) {
                meta = document.createElement('meta');
                meta.setAttribute('name', 'viewport');
                (document.head || document.documentElement).appendChild(meta);
              }
              meta.setAttribute('content', 'width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no, viewport-fit=cover');
              document.documentElement.style.webkitUserSelect = 'none';
              document.documentElement.style.webkitTouchCallout = 'none';
            })();
            """,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        configuration.userContentController.addUserScript(viewportScript)
        configuration.userContentController.add(context.coordinator, name: Coordinator.readyMessageName)

        configuration.preferences.javaScriptCanOpenWindowsAutomatically = true
        configuration.allowsInlineMediaPlayback = true
        configuration.mediaTypesRequiringUserActionForPlayback = []
        if #available(iOS 15.4, *) {
            configuration.preferences.isElementFullscreenEnabled = true
        }

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsLinkPreview = false
        webView.isOpaque = false
        webView.backgroundColor = UIColor(BattlePalette.background)
        webView.scrollView.backgroundColor = UIColor(BattlePalette.background)
        webView.scrollView.bounces = false
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        // Leaflet handles its own pan/zoom; the outer scroll view must not.
        webView.scrollView.isScrollEnabled = false
        webView.scrollView.maximumZoomScale = 1.0
        webView.scrollView.minimumZoomScale = 1.0

        context.coordinator.attach(webView)
        context.coordinator.load(url)
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        context.coordinator.updateCallbacks(onReady: onReady, onLoadFailure: onLoadFailure)
        if context.coordinator.reloadToken != reloadToken {
            context.coordinator.reload()
        } else if context.coordinator.currentURL != url {
            context.coordinator.load(url)
        }
        if context.coordinator.logNavigationToken != logNavigationToken {
            context.coordinator.openNetworkLog(token: logNavigationToken)
        }
    }

    static func dismantleUIView(_ webView: WKWebView, coordinator: Coordinator) {
        coordinator.tearDown()
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.uiDelegate = nil
    }

    // MARK: - Coordinator

    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {

        static let readyMessageName = "battleReady"

        /// Readiness probe from the interface contract, verbatim.
        static let readinessProbe = """
        (() => document.documentElement.dataset.battleReady === '1' && Boolean(document.querySelector('#app')?.childElementCount) && Boolean(document.querySelector('.leaflet-container')))()
        """

        /// In-page navigation request for the radar's network log panel. Written
        /// defensively so an older page build simply does nothing.
        static let networkLogScript = """
        (function () {
          try {
            var candidates = [
              '[data-battle-panel="network-log"]',
              '#network-log',
              '[data-panel="network-log"]',
              '[data-battle-action="network-log"]',
              '.battle-network-log'
            ];
            for (var i = 0; i < candidates.length; i++) {
              var el = document.querySelector(candidates[i]);
              if (el) {
                el.click();
                return 'clicked:' + candidates[i];
              }
            }
            var settings = document.querySelector('[data-battle-panel="settings"], #battle-settings, .battle-settings');
            if (settings) { settings.click(); }
            if (document.documentElement.dataset) {
              document.documentElement.dataset.battlePanel = 'network-log';
            }
            return 'fallback';
          } catch (error) {
            return 'error:' + error;
          }
        })();
        """

        private weak var webView: WKWebView?
        private var onReady: () -> Void
        private var onLoadFailure: (String) -> Void
        private let probeInterval: TimeInterval
        private let probeAttemptLimit: Int

        private(set) var reloadToken: Int
        private(set) var logNavigationToken: Int
        private(set) var currentURL: URL?

        private var hasSignalledReady = false
        private var didReportFailure = false
        private var probeTask: Task<Void, Never>?
        private var probeAttempts = 0

        /// Set by the page itself when it can reach the native side.
        private var pageDeclaredReady = false

        init(
            reloadToken: Int,
            logNavigationToken: Int,
            onReady: @escaping () -> Void,
            onLoadFailure: @escaping (String) -> Void,
            probeInterval: TimeInterval,
            probeAttemptLimit: Int
        ) {
            self.reloadToken = reloadToken
            self.logNavigationToken = logNavigationToken
            self.onReady = onReady
            self.onLoadFailure = onLoadFailure
            self.probeInterval = probeInterval
            self.probeAttemptLimit = probeAttemptLimit
            super.init()
        }

        func attach(_ webView: WKWebView) {
            self.webView = webView
        }

        func updateCallbacks(onReady: @escaping () -> Void, onLoadFailure: @escaping (String) -> Void) {
            self.onReady = onReady
            self.onLoadFailure = onLoadFailure
        }

        // MARK: Loading

        func load(_ url: URL?) {
            guard let url else { return }
            currentURL = url
            resetProbeState()
            webView?.load(Self.request(for: url))
        }

        func reload() {
            guard let webView else { return }
            guard let url = webView.url ?? currentURL else { return }
            currentURL = url
            resetProbeState()
            webView.load(Self.request(for: url))
        }

        /// "雷达设置 → 网络日志": the radar page lives in the same document as
        /// the map, so the shell asks it to switch panels instead of navigating
        /// away (which would drop the radar WebSocket).
        ///
        /// `updateNetworkLogToken` is called by the representable so repeated
        /// requests are not swallowed.
        func openNetworkLog(token: Int) {
            logNavigationToken = token
            guard let webView else { return }
            webView.evaluateJavaScript(Self.networkLogScript) { _, _ in }
        }

        func tearDown() {
            probeTask?.cancel()
            probeTask = nil
            webView?.configuration.userContentController.removeScriptMessageHandler(forName: Self.readyMessageName)
        }

        private static func request(for url: URL) -> URLRequest {
            URLRequest(
                url: url,
                cachePolicy: .reloadIgnoringLocalAndRemoteCacheData,
                timeoutInterval: 15
            )
        }

        private func resetProbeState() {
            hasSignalledReady = false
            didReportFailure = false
            pageDeclaredReady = false
            probeAttempts = 0
            probeTask?.cancel()
            probeTask = nil
        }

        // MARK: Readiness probe

        /// Polls the contract probe every 0.5 s, giving up after ~20 attempts.
        private func beginProbe() {
            guard !hasSignalledReady else { return }
            probeTask?.cancel()
            probeAttempts = 0
            probeTask = Task { @MainActor [weak self] in
                guard let self else { return }
                while !Task.isCancelled {
                    if self.probeAttempts >= self.probeAttemptLimit {
                        self.probeTask = nil
                        if !self.hasSignalledReady {
                            self.report("HTML 已载入但地图前端未完成挂载")
                        }
                        return
                    }
                    self.probeAttempts += 1
                    let ready = await self.runProbe()
                    if Task.isCancelled { return }
                    if ready || self.pageDeclaredReady {
                        self.markReady()
                        return
                    }
                    try? await Task.sleep(nanoseconds: UInt64(self.probeInterval * 1_000_000_000))
                }
            }
        }

        private func runProbe() async -> Bool {
            guard let webView else { return false }
            return await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
                webView.evaluateJavaScript(Self.readinessProbe) { result, _ in
                    if let value = result as? Bool {
                        continuation.resume(returning: value)
                    } else if let number = result as? NSNumber {
                        continuation.resume(returning: number.boolValue)
                    } else {
                        continuation.resume(returning: false)
                    }
                }
            }
        }

        private func markReady() {
            guard !hasSignalledReady else { return }
            hasSignalledReady = true
            probeAttempts = 0
            probeTask?.cancel()
            probeTask = nil
            onReady()
        }

        private func report(_ message: String) {
            guard !didReportFailure else { return }
            didReportFailure = true
            onLoadFailure(message)
        }

        // MARK: WKNavigationDelegate

        func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
            resetProbeState()
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            beginProbe()
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            guard (error as NSError).code != NSURLErrorCancelled else { return }
            probeTask?.cancel()
            probeTask = nil
            report(Self.describe(error))
        }

        func webView(
            _ webView: WKWebView,
            didFailProvisionalNavigation navigation: WKNavigation!,
            withError error: Error
        ) {
            guard (error as NSError).code != NSURLErrorCancelled else { return }
            probeTask?.cancel()
            probeTask = nil
            report(Self.describe(error))
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            resetProbeState()
            report("Web 内容进程已终止")
            // Recovery: the radar is served from 127.0.0.1, so an immediate
            // reload is cheap and beats leaving the user with a blank canvas.
            reload()
        }

        // MARK: WKScriptMessageHandler

        /// Optional native handshake used when the web layer can reach
        /// `window.webkit.messageHandlers.battleReady`.
        func userContentController(
            _ userContentController: WKUserContentController,
            didReceive message: WKScriptMessage
        ) {
            guard message.name == Self.readyMessageName else { return }
            pageDeclaredReady = true
            markReady()
        }

        // MARK: WKUIDelegate - native alerts for the radar page

        func webView(
            _ webView: WKWebView,
            runJavaScriptAlertPanelWithMessage message: String,
            initiatedByFrame frame: WKFrameInfo,
            completionHandler: @escaping () -> Void
        ) {
            let promise = Promise(completionHandler)
            guard let host = webView.window?.rootViewController ?? Self.topViewController() else {
                promise.resolve()
                return
            }
            let alert = UIAlertController(title: "雷达页面提示", message: message, preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "好", style: .default) { _ in promise.resolve() })
            host.present(alert, animated: true)
        }

        func webView(
            _ webView: WKWebView,
            runJavaScriptConfirmPanelWithMessage message: String,
            initiatedByFrame frame: WKFrameInfo,
            completionHandler: @escaping (Bool) -> Void
        ) {
            let promise = Promise(completionHandler)
            guard let host = webView.window?.rootViewController ?? Self.topViewController() else {
                promise.resolve(false)
                return
            }
            let alert = UIAlertController(title: "雷达页面确认", message: message, preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "取消", style: .cancel) { _ in promise.resolve(false) })
            alert.addAction(UIAlertAction(title: "确定", style: .default) { _ in promise.resolve(true) })
            host.present(alert, animated: true)
        }

        /// One-shot wrapper: WebKit hangs if a completion handler runs twice, and
        /// this also covers the "presentation failed" path.
        final class Promise {
            private var handler: ((Bool) -> Void)?
            private let queue = DispatchQueue.main

            init(_ handler: @escaping () -> Void) {
                self.handler = { _ in handler() }
            }

            init(_ handler: @escaping (Bool) -> Void) {
                self.handler = handler
            }

            func resolve(_ value: Bool = true) {
                let pending = handler
                handler = nil
                guard let pending else { return }
                if Thread.isMainThread {
                    pending(value)
                } else {
                    queue.async { pending(value) }
                }
            }
        }

        private static func topViewController() -> UIViewController? {
            let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
            let windows = scenes.flatMap { $0.windows }
            let window = windows.first { $0.isKeyWindow } ?? windows.first
            var controller = window?.rootViewController
            while let presented = controller?.presentedViewController {
                controller = presented
            }
            return controller
        }

        // MARK: Error copy

        private static func describe(_ error: Error) -> String {
            let nsError = error as NSError
            switch nsError.code {
            case NSURLErrorCannotConnectToHost, NSURLErrorNetworkConnectionLost:
                return "无法连接本机雷达端口，请确认接收器仍在运行"
            case NSURLErrorTimedOut:
                return "雷达页面加载超时，请重新启动接收服务"
            case NSURLErrorNotConnectedToInternet:
                return "网络连接已中断，请检查 Wi-Fi"
            case NSURLErrorCancelled:
                return "已取消加载"
            case NSURLErrorCannotFindHost, NSURLErrorBadURL:
                return "雷达网址无效：\(nsError.localizedDescription)"
            default:
                return "雷达页面加载失败：\(nsError.localizedDescription)"
            }
        }
    }
}
