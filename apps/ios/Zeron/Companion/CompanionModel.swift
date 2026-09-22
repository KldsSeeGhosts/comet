import Foundation
import Observation

struct HostChat: Decodable, Identifiable, Hashable {
    var id: String
    var deviceId: String
    var title: String?
    var archived: Bool
    var cwd: String?
    var branch: String?
    var spaceId: String?
    var config: ChatConfig?
    var lastMessagePreview: String?
    var lastMessageAt: String?
    var createdAt: String
    var displayTitle: String { title.flatMap { $0.isEmpty ? nil : $0 } ?? "New session" }
}

struct HostSpace: Decodable, Identifiable {
    let id: String
    let deviceId: String
    let path: String
    let name: String?
    var displayName: String { name ?? (path as NSString).lastPathComponent }
}

struct HostSession: Decodable {
    let chatId: String
    let status: String
}

struct HostHarness: Decodable, Identifiable {
    let id: String
    let name: String
    var label: String { name }
    let installed: Bool?
    let enabled: Bool?
}

struct HostAgentModel: Decodable, Identifiable {
    let id: String
    let label: String
    let reasoningLevels: [String]
}

struct HostPart: Codable, Identifiable, Equatable {
    var id: String
    var kind: String
    var text: String?
    var message: String?
    var call: JSONValue?
    var isError: Bool?
    var requestId: String?
    var questions: [UserInputQuestion]?
    var resolved: Bool?
}

struct HostMessage: Codable, Identifiable, Equatable {
    var id: String
    var role: String
    var status: String?
    var parts: [HostPart]
}

struct HostTranscriptFrame: Decodable {
    struct Upsert: Decodable { let after: String?; let entry: HostMessage }
    struct Append: Decodable { let entry: String; let part: String; let text: String; let len: Int }
    var reset: [HostMessage]?
    var upsert: [Upsert]?
    var append: [Append]?
    var remove: [String]?
    var count: Int?

    func applying(to source: [HostMessage]) throws -> [HostMessage] {
        if let reset { return reset }
        var rows = source
        rows.removeAll { (remove ?? []).contains($0.id) }
        for change in upsert ?? [] {
            rows.removeAll { $0.id == change.entry.id }
            if let anchor = change.after {
                guard let index = rows.firstIndex(where: { $0.id == anchor }) else { throw RelayError.rpc("Transcript needs a refresh.") }
                rows.insert(change.entry, at: index + 1)
            } else { rows.insert(change.entry, at: 0) }
        }
        for change in append ?? [] {
            guard let row = rows.firstIndex(where: { $0.id == change.entry }),
                  let part = rows[row].parts.firstIndex(where: { $0.id == change.part }) else { throw RelayError.rpc("Transcript needs a refresh.") }
            let text = (rows[row].parts[part].text ?? "") + change.text
            guard text.utf8.count == change.len else { throw RelayError.rpc("Transcript needs a refresh.") }
            rows[row].parts[part].text = text
        }
        guard count == rows.count else { throw RelayError.rpc("Transcript needs a refresh.") }
        return rows
    }
}

@MainActor @Observable
final class CompanionModel {
    var profiles: [ConnectionProfile] = []
    var selectedID: String? { didSet { UserDefaults.standard.set(selectedID, forKey: "companion.computer") } }
    var online = false
    var connectionMessage = "Connecting"
    var error: String?
    var chats: [HostChat] = []
    var spaces: [HostSpace] = []
    var sessions: [HostSession] = []
    var harnesses: [HostHarness] = []
    var generation = 0
    var connectionRevision = 0
    var drafts: [String: String] = [:]
    var connection = DirectConnection()
    var selected: ConnectionProfile? { profiles.first { $0.id == selectedID } }
    var localChats: [HostChat] {
        chats.filter { !$0.archived && $0.deviceId == selected?.deviceId }
            .sorted { ($0.lastMessageAt ?? $0.createdAt) > ($1.lastMessageAt ?? $1.createdAt) }
    }
    var archivedChats: [HostChat] {
        chats.filter { $0.archived && $0.deviceId == selected?.deviceId }
            .sorted { ($0.lastMessageAt ?? $0.createdAt) > ($1.lastMessageAt ?? $1.createdAt) }
    }
    func visibleChats(project: String = "", query: String = "", scope: CompanionScope = .all) -> [HostChat] {
        let source = scope == .archived ? archivedChats : localChats
        let search = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return source.filter { chat in
            (project.isEmpty || chat.spaceId == project)
                && (scope != .attention || ["awaitingInput", "errored"].contains(status(chat.id)))
                && (scope != .working || status(chat.id) == "working")
                && (search.isEmpty || [chat.displayTitle, chat.lastMessagePreview ?? "", chat.branch ?? "",
                    chat.cwd ?? "", localSpaces.first { $0.id == chat.spaceId }?.displayName ?? ""]
                    .contains { $0.localizedCaseInsensitiveContains(search) })
        }.sorted {
            let a = Self.attentionRank(status($0.id)), b = Self.attentionRank(status($1.id))
            if scope != .archived && a != b { return a < b }
            return ($0.lastMessageAt ?? $0.createdAt) > ($1.lastMessageAt ?? $1.createdAt)
        }
    }
    private static func attentionRank(_ status: String) -> Int {
        switch status { case "awaitingInput": 0; case "errored": 1; case "working": 2; case "completed": 3; default: 4 }
    }
    func draftKey(for chat: HostChat) -> String { "\(chat.deviceId)/\(chat.id)" }

    var localSpaces: [HostSpace] { spaces.filter { $0.deviceId == selected?.deviceId } }

    init() {
        do { profiles = try CompanionKeychain.load() }
        catch { self.error = error.localizedDescription }
        let saved = UserDefaults.standard.string(forKey: "companion.computer")
        selectedID = profiles.first { $0.id == saved }?.id ?? profiles.first?.id
    }

    func pair(_ code: String) throws {
        let profile = try ConnectionProfile.parse(code)
        var updated = profiles.filter { $0.id != profile.id }
        updated.append(profile)
        try CompanionKeychain.save(updated)
        profiles = updated
        select(profile.id)
    }

    func select(_ id: String?) {
        connection.close()
        online = false
        chats = []; spaces = []; sessions = []; harnesses = []
        selectedID = id
        error = nil
        connectionMessage = "Connecting"
        connectionRevision += 1
        generation += 1
    }

    func forget(_ profile: ConnectionProfile) throws {
        let updated = profiles.filter { $0.id != profile.id }
        try CompanionKeychain.save(updated)
        profiles = updated
        if selectedID == profile.id { select(updated.first?.id) }
    }

    func maintainConnection() async {
        guard let profile = selected else { return }
        // Each lifecycle owns its connection. Cancellation of an old host or
        // background task must never close the replacement connection.
        let connection = DirectConnection()
        self.connection = connection
        await withTaskCancellationHandler {
            await maintain(profile, using: connection)
        } onCancel: {
            Task { @MainActor in connection.close() }
        }
    }

    private func maintain(_ profile: ConnectionProfile, using connection: DirectConnection) async {
        while !Task.isCancelled {
            online = false
            connectionMessage = "Connecting"
            do {
                try await connection.connect(profile)
                try Task.checkCancellation()
                connectionMessage = "Loading sessions"
                var snapshots: Set<String> = []
                func arrived(_ name: String) {
                    snapshots.insert(name)
                    guard snapshots.count == 4, !online else { return }
                    online = true
                    connectionMessage = "Connected"
                    error = nil
                    generation += 1
                }
                try await withThrowingTaskGroup(of: Void.self) { group in
                    group.addTask { @MainActor in
                        let catalog = try await connection.call("ListHarnesses")
                        guard !Task.isCancelled, self.connection === connection else { return }
                        self.harnesses = try Self.decode([HostHarness].self, catalog).filter { $0.enabled ?? ($0.installed ?? true) }
                        arrived("agents")
                    }
                    group.addTask { @MainActor in
                        for try await value in try await connection.watch("WatchChats") {
                            guard !Task.isCancelled, self.connection === connection else { return }
                            self.chats = try Self.decode([HostChat].self, value)
                            arrived("chats")
                        }
                        throw RelayError.notConnected
                    }
                    group.addTask { @MainActor in
                        for try await value in try await connection.watch("WatchSpaces") {
                            guard !Task.isCancelled, self.connection === connection else { return }
                            self.spaces = try Self.decode([HostSpace].self, value)
                            arrived("spaces")
                        }
                        throw RelayError.notConnected
                    }
                    group.addTask { @MainActor in
                        for try await value in try await connection.watch("WatchSessions") {
                            guard !Task.isCancelled, self.connection === connection else { return }
                            self.sessions = try Self.decode([HostSession].self, value)
                            arrived("sessions")
                        }
                        throw RelayError.notConnected
                    }
                    do { for try await _ in group {} }
                    catch { connection.close(); group.cancelAll(); throw error }
                }
            } catch {
                connection.close()
                guard !Task.isCancelled, self.connection === connection else { return }
                online = false
                connectionMessage = "Reconnecting"
                self.error = error.localizedDescription
            }
            do { try await Task.sleep(for: .seconds(4)) } catch { return }
        }
    }

    func status(_ chatID: String) -> String { sessions.first { $0.chatId == chatID }?.status ?? "idle" }

    func create(spaceID: String?, harness: String, model: String? = nil, reasoning: String? = nil) async throws -> HostChat {
        guard online, let host = selected else { throw RelayError.notConnected }
        guard harnesses.contains(where: { $0.id == harness }),
              spaceID == nil || localSpaces.contains(where: { $0.id == spaceID }) else {
            throw RelayError.rpc("Refresh the computer's projects and agents before creating a session.")
        }
        let id = UUID().uuidString.lowercased()
        var config: [String: Any] = ["harness": harness, "sandbox": "workspace-write", "modelOptions": [:]]
        config["model"] = model
        config["reasoning"] = reasoning
        var params: [String: Any] = ["op": "createChat", "chatId": id, "deviceId": host.deviceId, "config": config]
        if let spaceID { params["spaceId"] = spaceID }
        _ = try await connection.call("Mutate", params)
        return HostChat(id: id, deviceId: host.deviceId, archived: false,
                        cwd: localSpaces.first { $0.id == spaceID }?.path ?? "~", spaceId: spaceID,
                        config: ChatConfig(harness: harness, model: model, reasoning: reasoning, sandbox: "workspace-write"), createdAt: ISO8601DateFormatter().string(from: Date()))
    }

    func read(_ method: String, chat: HostChat, params: [String: Any]) async throws -> JSONValue {
        guard online, chat.deviceId == selected?.deviceId else { throw RelayError.notConnected }
        return try await connection.call(method, params)
    }

    func models(for harness: String) async throws -> [HostAgentModel] {
        guard online else { throw RelayError.notConnected }
        return try Self.decode([HostAgentModel].self, await connection.call("ListModels", ["harness": harness]))
    }

    func rename(_ chat: HostChat, title: String) async throws {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty else { throw RelayError.rpc("Enter a session name.") }
        try await mutate(chat, values: ["op": "renameChat", "title": title])
    }

    func archive(_ chat: HostChat, archived: Bool) async throws {
        try await mutate(chat, values: ["op": "setChatArchived", "archived": archived])
    }

    private func mutate(_ chat: HostChat, values: [String: Any]) async throws {
        guard online, chat.deviceId == selected?.deviceId else { throw RelayError.notConnected }
        var params = values
        params["chatId"] = chat.id
        // WatchChats owns local state. A failed or ambiguous write must never
        // optimistically hide a session or get replayed after reconnecting.
        _ = try await connection.call("Mutate", params)
    }

    func send(_ text: String, chat: HostChat, messageID: String = UUID().uuidString.lowercased()) async throws {
        guard online, chat.deviceId == selected?.deviceId else { throw RelayError.notConnected }
        if status(chat.id) == "working" || status(chat.id) == "awaitingInput" {
            _ = try await connection.call("QueueMessage", ["chatId": chat.id, "text": text, "holdForTurnEnd": true])
        } else {
            var request: [String: Any] = ["prompt": text, "cwd": chat.cwd ?? "~",
                "sandbox": chat.config?.sandbox ?? "workspace-write", "autoApprove": false]
            if let config = chat.config {
                request["harness"] = config.harness
                request["model"] = config.model
                request["reasoning"] = config.reasoning
                request["modelOptions"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(config.modelOptions))
            }
            _ = try await connection.call("QueueCommand", ["chatId": chat.id,
                "command": ["kind": "run", "messageId": messageID, "request": request]])
        }
    }

    func respond(requestID: String, answers: [UserInputAnswer], chat: HostChat) async throws {
        guard online, chat.deviceId == selected?.deviceId else { throw RelayError.notConnected }
        let encoded = try JSONSerialization.jsonObject(with: JSONEncoder().encode(answers))
        _ = try await connection.call("QueueCommand", ["chatId": chat.id,
            "command": ["kind": "respondInput", "requestId": requestID, "answers": encoded]])
    }

    func stop(_ chat: HostChat) async throws {
        guard online, chat.deviceId == selected?.deviceId else { throw RelayError.notConnected }
        _ = try await connection.call("QueueCommand", ["chatId": chat.id, "command": ["kind": "interrupt"]])
    }

    static func decode<T: Decodable>(_ type: T.Type, _ value: JSONValue) throws -> T {
        try JSONDecoder().decode(type, from: JSONEncoder().encode(value))
    }
}

enum CompanionScope: String, CaseIterable {
    case all = "All", attention = "Needs you", working = "Working", archived = "Archived"
}
