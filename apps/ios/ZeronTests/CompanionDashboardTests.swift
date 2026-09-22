import XCTest
@testable import Zeron

@MainActor
final class CompanionDashboardTests: XCTestCase {
    private func model() -> CompanionModel {
        let model = CompanionModel()
        model.profiles = [ConnectionProfile(id: "test-host", name: "Studio", endpoint: "ws://127.0.0.1:28777", token: String(repeating: "a", count: 64), deviceId: "mac")]
        model.selectedID = "test-host"
        model.spaces = [HostSpace(id: "project", deviceId: "mac", path: "/code/noches", name: "Noches")]
        model.chats = [
            HostChat(id: "idle", deviceId: "mac", title: "Newest session", archived: false, branch: "fix/keychain", createdAt: "2026-09-22"),
            HostChat(id: "working", deviceId: "mac", title: "Desktop sidebar", archived: false, spaceId: "project", createdAt: "2026-09-21"),
            HostChat(id: "attention", deviceId: "mac", title: "Approve migration", archived: false, spaceId: "project", createdAt: "2026-09-20"),
            HostChat(id: "archive", deviceId: "mac", title: "Old work", archived: true, createdAt: "2026-09-19"),
            HostChat(id: "remote", deviceId: "other", title: "Other host", archived: false, createdAt: "2026-09-23")
        ]
        model.sessions = [HostSession(chatId: "working", status: "working"), HostSession(chatId: "attention", status: "awaitingInput")]
        return model
    }
    func testAttentionSortingAndHostIsolation() {
        let model = model()
        XCTAssertEqual(model.visibleChats().map(\.id), ["attention", "working", "idle"])
        XCTAssertEqual(model.visibleChats(scope: .attention).map(\.id), ["attention"])
        XCTAssertEqual(model.visibleChats(scope: .working).map(\.id), ["working"])
        XCTAssertEqual(model.visibleChats(scope: .archived).map(\.id), ["archive"])
    }
    func testSearchCombinesProjectScopeAndMetadata() {
        let model = model()
        XCTAssertEqual(model.visibleChats(query: " KEYCHAIN ").map(\.id), ["idle"])
        XCTAssertEqual(model.visibleChats(query: "noches").map(\.id), ["attention", "working"])
        XCTAssertTrue(model.visibleChats(project: "project", query: "keychain").isEmpty)
        XCTAssertEqual(model.visibleChats(project: "project", scope: .working).map(\.id), ["working"])
    }
    func testSelectingSameHostRestartsConnectionAndDraftsStayScoped() {
        let model = model()
        let chat = model.chats[0]
        var other = chat; other.deviceId = "other"
        model.drafts[model.draftKey(for: chat)] = "Keep this draft"
        let revision = model.connectionRevision
        model.select(model.selectedID)
        XCTAssertEqual(model.connectionRevision, revision + 1)
        XCTAssertEqual(model.drafts[model.draftKey(for: chat)], "Keep this draft")
        XCTAssertNil(model.drafts[model.draftKey(for: other)])
        XCTAssertFalse(model.online)
    }
    func testTranscriptAdapterPreservesToolResolutionAndApprovalIdentity() throws {
        let json = #"{"id":"message","role":"assistant","status":"streaming","parts":[{"id":"tool","kind":"tool","call":{"kind":"exec","command":"swift test"},"isError":false},{"id":"question","kind":"input","requestId":"approval-17","questions":[{"id":"q","header":"Approve","question":"Run tests?","options":["Allow"]}],"resolved":false}]}"#
        let message = try JSONDecoder().decode(HostMessage.self, from: Data(json.utf8))
        let entry = message.renderedEntry
        XCTAssertEqual(entry.status, .streaming)
        guard case .tool(_, let call, let isError, let resolved) = entry.parts[0] else { return XCTFail("Missing tool") }
        XCTAssertEqual(call.string("command"), "swift test")
        XCTAssertFalse(isError); XCTAssertTrue(resolved)
        guard case .input(_, let request, let questions, let answered) = entry.parts[1] else { return XCTFail("Missing approval") }
        XCTAssertEqual(request, "approval-17")
        XCTAssertEqual(questions.first?.options, ["Allow"])
        XCTAssertFalse(answered)
    }

    func testOfflineAndWrongHostCannotMutate() async {
        let model = model()
        do { try await model.rename(model.chats[0], title: "Rename"); XCTFail("Offline") } catch {}
        model.online = true
        do { try await model.archive(model.chats.last!, archived: true); XCTFail("Wrong host") } catch {}
        do { try await model.rename(model.chats[0], title: " \n "); XCTFail("Blank title") } catch {}
    }
}
