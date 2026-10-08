//
//  BonjourAdvertiser.swift
//  BattleReceiverOpen
//
//  Publishes the receiver on the local link so device B (the phone running the
//  game) can discover it without typing an IP.
//
//  Service types are the contract values from docs/INTERFACES.md / Info.plist:
//    _battleproxy._tcp.   SOCKS5 control channel
//    _battleproxy._udp.   UDP relay (per-source-port NAT)
//
//  iOS 14+ requires com.apple.developer.networking.multicast for mDNS traffic,
//  which is declared in BattleReceiverOpen.entitlements.
//

import Foundation

/// Publishes both the TCP and UDP variants of `_battleproxy`.
/// All NSNetServiceDelegate callbacks are no-ops by design: name collisions are
/// resolved by the system (NSNetService automatically renames to "name (2)"),
/// and publication failures must not abort the receiver startup path.
final class BonjourAdvertiser: NSObject {

    /// Bonjour caps the service type at 15 characters, including the underscore.
    static let tcpServiceType = "_battleproxy._tcp."
    static let udpServiceType = "_battleproxy._udp."

    private var services: [NetService] = []
    private(set) var isAdvertising = false
    private var advertisedPort: UInt16 = 0
    private let brand: String

    init(brand: String = BattleBrand.current) {
        self.brand = brand
        super.init()
    }

    /// Human readable service name; kept stable so a second launch replaces the
    /// stale record instead of stacking "(2)" duplicates.
    var serviceName: String {
        "BattleReceiverOpen-\(brand)"
    }

    /// Start advertising on `port`. Idempotent: calling twice with the same port
    /// is ignored, and calling with a new port republishes.
    func start(port: UInt16) {
        guard port > 0 else { return }
        if isAdvertising && advertisedPort == port { return }
        stop()

        advertisedPort = port
        let txt = txtRecord(port: port)

        let tcp = NetService(
            domain: "local.",
            type: Self.tcpServiceType,
            name: serviceName,
            port: Int32(port)
        )
        let udp = NetService(
            domain: "local.",
            type: Self.udpServiceType,
            name: serviceName,
            port: Int32(port)
        )

        for service in [tcp, udp] {
            service.delegate = self
            service.includesPeerToPeer = true
            if let txt {
                _ = service.setTXTRecord(txt)
            }
            service.schedule(in: .main, forMode: .common)
            service.publish(options: [.listenForConnections])
        }

        services = [tcp, udp]
        isAdvertising = true
    }

    /// Unpublish everything and release the delegates (breaks the retain cycle
    /// NSNetService otherwise holds on its delegate).
    func stop() {
        guard !services.isEmpty else {
            isAdvertising = false
            advertisedPort = 0
            return
        }
        for service in services {
            service.stop()
            service.remove(from: .main, forMode: .common)
            service.delegate = nil
        }
        services.removeAll()
        isAdvertising = false
        advertisedPort = 0
    }

    // MARK: TXT record

    private func txtRecord(port: UInt16) -> Data? {
        var fields: [(String, String)] = [
            ("brand", brand),
            ("ver", BattleBridge.shared.version),
            ("socks", "socks5"),
            ("udp", "1")
        ]
        if let address = BattleBridge.localIPv4Address() {
            fields.append(("ip", address))
            fields.append(("radar", "/battle.html?brand=\(brand)"))
        }
        _ = port
        var dictionary: [String: Data] = [:]
        for (key, value) in fields {
            dictionary[key] = Data(value.utf8)
        }
        return NetService.data(fromTXTRecord: dictionary)
    }
}

// MARK: - NetServiceDelegate (intentional no-ops)

extension BonjourAdvertiser: NetServiceDelegate {

    /// A name collision is not an error: the system renames the instance, and
    /// the radar URL never depends on the advertised name.
    func netServiceWillPublish(_ sender: NetService) {
        // no-op: collision handling is delegated to the system
    }

    func netServiceDidPublish(_ sender: NetService) {
        // no-op: publishing is advisory, the receiver already binds the port
    }

    func netService(_ sender: NetService, didNotPublish errorDict: [String: NSNumber]) {
        // no-op: the failure view is driven by the C ABI status, not by mDNS
    }

    func netServiceWillResolve(_ sender: NetService) {
        // no-op
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        // no-op
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        // no-op
    }

    func netServiceDidStop(_ sender: NetService) {
        // no-op
    }

    func netService(_ sender: NetService, didUpdateTXTRecord data: Data) {
        // no-op
    }

    func netService(_ sender: NetService, didAcceptConnectionWith inputStream: InputStream, outputStream: OutputStream) {
        // no-op: NSNetService only accepts connections for `listenForConnections`
        // in the legacy stream API, which this receiver does not use.
        inputStream.close()
        outputStream.close()
    }
}
