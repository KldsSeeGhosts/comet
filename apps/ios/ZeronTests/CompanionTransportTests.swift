import XCTest
@testable import Zeron

/// Run scripts/fixtures/noches-mobile-host.mjs before these tests.
@MainActor
final class CompanionTransportTests: XCTestCase {
    private var profile: ConnectionProfile {
        ConnectionProfile(id: "mobile-fixture", name: "Studio fixture", endpoint: "ws://127.0.0.1:28777",
                          token: String(repeating: "a", count: 64), deviceId: "fixture-mac")
    }
    private func requireFixture() async throws {
        var request = URLRequest(url: URL(string: "http://127.0.0.1:28777/health")!)
        request.timeoutInterval = 2
        do {
            let (data, _) = try await URLSession.shared.data(for: request)
            guard String(decoding: data, as: UTF8.self).contains("fixture") else { throw RelayError.notConnected }
        } catch { throw XCTSkip("Start scripts/fixtures/noches-mobile-host.mjs to run integration tests.") }
    }
    /// Opt-in, read-only test. Place an existing pairing code in the simulator
    /// app's Documents/companion-live.code; the test never saves it to Keychain.
    func testLivePairedHostWhenConfigured() async throws {
        let url = try XCTUnwrap(FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first)
            .appendingPathComponent("companion-live.code")
        guard FileManager.default.fileExists(atPath: url.path) else { throw XCTSkip("No live host configured.") }
        let host = try ConnectionProfile.parse(String(contentsOf: url, encoding: .utf8))
        let connection = DirectConnection()
        defer { connection.close() }
        try await connection.connect(host)
        let info = try await connection.call("EngineInfo")
        XCTAssertEqual(info.objectValue?["deviceId"]?.stringValue, host.deviceId)
        let stream = try await connection.watch("WatchChats")
        var iterator = stream.makeAsyncIterator()
        let snapshot = try await iterator.next()
        let value = try XCTUnwrap(snapshot)
        let chats = try CompanionModel.decode([HostChat].self, value)
        XCTAssertTrue(chats.allSatisfy { !$0.id.isEmpty })
    }

    func testIdentityCatalogAndLiveSnapshot() async throws {
        try await requireFixture()
        let connection = DirectConnection()
        defer { connection.close() }
        try await connection.connect(profile)
        let value = try await connection.call("ListHarnesses")
        let harnesses = try CompanionModel.decode([HostHarness].self, value)
        XCTAssertEqual(harnesses.first?.label, "Fixture agent")
        let stream = try await connection.watch("WatchChats")
        var iterator = stream.makeAsyncIterator()
        let next = try await iterator.next()
        let snapshot = try XCTUnwrap(next)
        XCTAssertEqual(try CompanionModel.decode([HostChat].self, snapshot).first?.deviceId, profile.deviceId)
        // An idle healthy link must survive beyond the request/snapshot deadline.
        try await Task.sleep(for: .seconds(21))
        let info = try await connection.call("EngineInfo")
        XCTAssertEqual(info.objectValue?["deviceId"]?.stringValue, profile.deviceId)
    }
    func testRealRustGatewayChunksAndFreshReconnects() async throws {
        var probe = URLRequest(url: URL(string: "http://127.0.0.1:28778/health")!)
        probe.timeoutInterval = 2
        do { _ = try await URLSession.shared.data(for: probe) }
        catch { throw XCTSkip("Start fixture --engine and the Rust gateway on 28779.") }
        let host = ConnectionProfile(id: profile.id, name: profile.name, endpoint: "ws://127.0.0.1:28779",
                                     token: profile.token, deviceId: profile.deviceId)
        for _ in 0..<3 {
            let connection = DirectConnection()
            do {
                try await connection.connect(host)
                let reply = try await connection.call("BigReply")
                XCTAssertEqual(reply.objectValue?["text"]?.stringValue, String(repeating: "🌙", count: 15000))
                connection.close()
            } catch { connection.close(); throw error }
        }
    }
    func testLargeUnicodeReplyReassemblesAcrossGatewayChunks() async throws {
        try await requireFixture()
        let connection = DirectConnection()
        defer { connection.close() }
        try await connection.connect(profile)
        let reply = try await connection.call("BigReply")
        XCTAssertEqual(reply.objectValue?["text"]?.stringValue, String(repeating: "🌙", count: 15000))
    }
    func testWrongKeyAndWrongIdentityAreRejected() async throws {
        try await requireFixture()
        let connection = DirectConnection()
        defer { connection.close() }
        for bad in [ConnectionProfile(id: profile.id, name: profile.name, endpoint: profile.endpoint,
                                       token: String(repeating: "b", count: 64), deviceId: profile.deviceId),
                    ConnectionProfile(id: profile.id, name: profile.name, endpoint: profile.endpoint,
                                       token: profile.token, deviceId: "another-host")] {
            do { try await connection.connect(bad); XCTFail("Should reject this profile") }
            catch { /* expected */ }
        }
    }
    func testLostMutationReplyIsNeverReplayed() async throws {
        try await requireFixture()
        let connection = DirectConnection()
        defer { connection.close() }
        try await connection.connect(profile)
        let nonce = UUID().uuidString
        do {
            _ = try await connection.call("FixtureDropMutation", ["nonce": nonce])
            XCTFail("The fixture closes before acknowledging")
        } catch { /* delivery is deliberately ambiguous */ }
        try await connection.connect(profile)
        let history = try await connection.call("FixtureCommands")
        let matching = history.arrayValue?.filter { $0.objectValue?["params"]?.objectValue?["nonce"]?.stringValue == nonce }
        XCTAssertEqual(matching?.count, 1)
    }
    func testRunStopAndApprovalWireShapes() async throws {
        try await requireFixture()
        let connection = DirectConnection()
        defer { connection.close() }
        try await connection.connect(profile)
        let run: [String: Any] = ["kind": "run", "messageId": UUID().uuidString,
            "request": ["prompt": "Fixture test", "cwd": "/tmp/noches-fixture", "sandbox": "workspace-write", "autoApprove": false]]
        let commands: [[String: Any]] = [run, ["kind": "interrupt"],
            ["kind": "respondInput", "requestId": "request-1", "answers": [["questionId": "q1", "labels": ["Allow"]]]]]
        for command in commands {
            let response = try await connection.call("QueueCommand", ["chatId": "fixture-chat", "command": command])
            XCTAssertNotNil(response.objectValue?["commandId"]?.stringValue)
        }
    }
}
