import Foundation

/// A fresh RPC session on each reconnect. Reads resubscribe; mutations are never
/// replayed after ambiguous delivery. The gateway pins the engine before welcome.
@MainActor
final class DirectConnection {
    private var socket: URLSessionWebSocketTask?
    private var reader: Task<Void, Never>?
    private var heartbeat: Task<Void, Never>?
    private var deadline: Task<Void, Never>?
    private let pending = DeviceRpcPending()
    private var nextID: UInt64 = 0
    private var sent: UInt64 = 0
    private var received: UInt64 = 0
    private var incomingPayload = Data()
    private var lastFrame = Date()
    private var awaitingSnapshots: Set<UInt64> = []

    func connect(_ profile: ConnectionProfile) async throws {
        close()
        try profile.validate()
        var request = URLRequest(url: URL(string: profile.endpoint)!)
        request.setValue("Bearer \(profile.token)", forHTTPHeaderField: "Authorization")
        request.timeoutInterval = 12
        let ws = URLSession.shared.webSocketTask(with: request)
        ws.maximumMessageSize = 32 * 1024 * 1024
        socket = ws
        sent = 0
        received = 0
        ws.resume()
        deadline = Task { [weak self] in
            try? await Task.sleep(for: .seconds(12))
            guard !Task.isCancelled else { return }
            self?.close(error: .timeout)
        }
        // Reuse a phone-local slot so fresh read subscriptions do not consume
        // a gateway retention slot on every foreground/reconnect. A previously
        // acknowledged session cannot resume from cursor zero; the gateway
        // replaces it with a fresh upstream session without replaying commands.
        let slotKey = "companion.session." + profile.id
        let sessionID = UserDefaults.standard.string(forKey: slotKey) ?? UUID().uuidString.lowercased()
        UserDefaults.standard.set(sessionID, forKey: slotKey)
        do {
            try await send(["t": "hello", "version": 1, "session": sessionID, "cursor": 0, "resume": false])
            let welcome = try await read(ws)
            if welcome["resumed"] as? Bool == true {
                // No prior server frame was acknowledged, so the gateway
                // cannot distinguish this fresh reader from a resume. Retire
                // this slot rather than accept replies to old RPC identifiers.
                UserDefaults.standard.removeObject(forKey: slotKey)
                throw RelayError.rpc("Refreshing the connection. Previous commands will not be repeated.")
            }
            guard welcome["t"] as? String == "welcome", welcome["version"] as? Int == 1,
                  welcome["resumed"] as? Bool == false, welcome["received"] as? Int == 0 else {
                throw RelayError.rpc("This host needs a compatible Noches connection gateway.")
            }
            deadline?.cancel()
            lastFrame = Date()
            reader = Task { [weak self] in
                do {
                    while !Task.isCancelled {
                        let frame = try await Self.readFrame(ws)
                        guard let self, self.socket === ws else { return }
                        self.lastFrame = Date()
                        try await self.handle(frame)
                    }
                } catch {
                    guard let self, self.socket === ws else { return }
                    self.close(error: (error as? RelayError) ?? .rpc("Connection lost. Check the session before resending a message."))
                }
            }
            heartbeat = Task { [weak self] in
                while !Task.isCancelled {
                    do {
                        try await Task.sleep(for: .seconds(8))
                        guard let self, self.socket === ws else { return }
                        guard Date().timeIntervalSince(self.lastFrame) < 24 else { throw RelayError.timeout }
                        try await self.send(["t": "ping"])
                    } catch {
                        guard !Task.isCancelled else { return }
                        self?.close(error: .timeout)
                        return
                    }
                }
            }
            let identity = try await call("EngineInfo")
            guard identity.objectValue?["deviceId"]?.stringValue == profile.deviceId else {
                throw RelayError.rpc("The computer's identity changed. Pair it again.")
            }
        } catch {
            close()
            throw error
        }
    }

    func close(error: RelayError = .notConnected) {
        deadline?.cancel(); deadline = nil
        reader?.cancel(); reader = nil
        heartbeat?.cancel(); heartbeat = nil
        socket?.cancel(with: .goingAway, reason: nil); socket = nil
        incomingPayload.removeAll(keepingCapacity: false)
        awaitingSnapshots.removeAll()
        pending.failAll(error: error)
    }

    func call(_ method: String, _ params: [String: Any] = [:]) async throws -> JSONValue {
        guard let requestSocket = socket else { throw RelayError.notConnected }
        try Task.checkCancellation()
        nextID += 1
        let id = nextID
        let data: Data = try await withTaskCancellationHandler {
           try await withCheckedThrowingContinuation { continuation in
               pending.registerUnary(id: id) { continuation.resume(with: $0) }
                Task { [weak self] in
                    guard let self, self.pending.owns(id: id), self.socket === requestSocket else { return }
                    do { try await self.rpc(["id": id, "method": method, "params": params]) }
                    catch {
                        if self.socket === requestSocket {
                            self.close(error: .rpc("Delivery was not confirmed. Check the session before resending."))
                        }
                    }
                }
               Task { [weak self] in
                    try? await Task.sleep(for: .seconds(20))
                    guard let self, self.socket === requestSocket, self.pending.owns(id: id) else { return }
                    self.close(error: .rpc("The host did not confirm delivery. Check the session before resending."))
                }
            }
        } onCancel: {
            Task { @MainActor [weak self] in
                self?.pending.fail(id: id, error: .rpc("Request cancelled. Check the session before resending."))
            }
        }
        return try JSONDecoder().decode(JSONValue.self, from: data)
    }

    func watch(_ method: String, _ params: [String: Any] = [:]) async throws -> AsyncThrowingStream<JSONValue, Error> {
        guard let requestSocket = socket else { throw RelayError.notConnected }
        nextID += 1
        let id = nextID
        let stream = pending.registerStream(id: id, as: JSONValue.self) { [weak self] in
            Task { @MainActor in
                guard let self, self.pending.removeStreamForCancellation(id: id) else { return }
                self.awaitingSnapshots.remove(id)
                try? await self.rpc(["id": id, "cancel": true])
            }
        }
        awaitingSnapshots.insert(id)
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(20))
            guard let self, self.socket === requestSocket, self.awaitingSnapshots.contains(id) else { return }
            self.close(error: .timeout)
        }
        do { try await rpc(["id": id, "method": method, "params": params]) }
        catch { pending.fail(id: id, error: .notConnected); throw error }
        return stream
    }

    private func rpc(_ object: [String: Any]) async throws {
        let data = try JSONSerialization.data(withJSONObject: object)
        sent += 1
        try await send(["t": "data", "seq": sent, "payload": String(decoding: data, as: UTF8.self), "end": true])
    }

    private func handle(_ frame: [String: Any]) async throws {
        switch frame["t"] as? String {
        case "data":
            guard let seq = (frame["seq"] as? NSNumber)?.uint64Value,
                  seq == received + 1, let payload = frame["payload"] as? String else {
                throw RelayError.rpc("The connection lost its place. Reconnecting to refresh.")
            }
            guard let end = frame["end"] as? Bool else {
                throw RelayError.rpc("The host is using an incompatible connection protocol.")
            }
            received = seq
            incomingPayload.append(contentsOf: payload.utf8)
            guard incomingPayload.count <= 32 * 1024 * 1024 else {
                throw RelayError.rpc("The host reply exceeds the connection limit.")
            }
            if end {
                let complete = incomingPayload
                incomingPayload = Data()
                for line in String(decoding: complete, as: UTF8.self).split(separator: "\n") {
                    guard let reply = try JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                          let id = (reply["id"] as? NSNumber)?.uint64Value else {
                        throw RelayError.rpc("The host sent an invalid reply.")
                    }
                    if reply["item"] != nil || reply["err"] != nil || reply["done"] != nil {
                        awaitingSnapshots.remove(id)
                    }
                }
                pending.handlePayload(complete)
            }
            try await send(["t": "ack", "seq": seq])
        case "ack", "pong": break
        case "reset": throw RelayError.rpc("The host restarted or reset the connection. Refreshing session state.")
        default: throw RelayError.notConnected
        }
    }

    private func send(_ frame: [String: Any]) async throws {
        guard let socket else { throw RelayError.notConnected }
        let data = try JSONSerialization.data(withJSONObject: frame)
        try await socket.send(.string(String(decoding: data, as: UTF8.self)))
    }

    private func read(_ ws: URLSessionWebSocketTask) async throws -> [String: Any] { try await Self.readFrame(ws) }
    private static func readFrame(_ ws: URLSessionWebSocketTask) async throws -> [String: Any] {
        let message = try await ws.receive()
        let data: Data
        switch message {
        case .string(let text): data = Data(text.utf8)
        case .data(let bytes): data = bytes
        @unknown default: throw RelayError.notConnected
        }
        guard let frame = try JSONSerialization.jsonObject(with: data) as? [String: Any] else { throw RelayError.notConnected }
        return frame
    }
}
