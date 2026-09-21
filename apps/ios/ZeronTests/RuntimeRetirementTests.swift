import XCTest
import Loro
import UIKit
@testable import Zeron

private actor RefreshGate {
    var calls = 0
    var continuation: CheckedContinuation<AuthTokens, Never>?
    func refresh() async -> AuthTokens {
        calls += 1
        return await withCheckedContinuation { continuation = $0 }
    }
    func finish() { continuation?.resume(returning: AuthTokens(accessToken: "new", refreshToken: "rotated")); continuation = nil }
}
private final class TokenWrites: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [AuthTokens] = []
    func save(_ value: AuthTokens) { lock.withLock { values.append(value) } }
    var count: Int { lock.withLock { values.count } }
}

final class RuntimeRetirementTests: XCTestCase {
    private let expired = "e30.eyJleHAiOjB9.sig"

    func testRetirementRejectsLateRefreshAndCredentialWrite() async {
        let gate = RefreshGate()
        let writes = TokenWrites()
        let config = AppConfig(edgeURL: URL(string: "https://example.invalid")!, mode: .workos,
            userId: "old", orgId: "org", deviceId: "ios", deviceName: "test",
            tokens: AuthTokens(accessToken: expired, refreshToken: "old-refresh"),
            refreshTokens: { _, _ in await gate.refresh() }, persistTokens: { writes.save($0) })
        let task = Task { await config.currentToken() }
        for _ in 0..<1000 {
            if await gate.calls > 0 { break }
            await Task.yield()
        }
        let calls = await gate.calls
        XCTAssertEqual(calls, 1)
        guard calls == 1 else { config.retire(); task.cancel(); return }
        config.retire()
        await gate.finish()
        let token = await task.value
        XCTAssertNil(token)
        XCTAssertEqual(writes.count, 0)
        let after = await config.currentToken()
        XCTAssertNil(after)
    }

    func testTerminalRejectionStopsRefreshRetriesButTransientFailureDoesNot() async {
        for (body, terminal) in [("{\"code\":\"invalid_grant\"}", true), ("{\"error\":\"authentication failed\"}", false)] {
            let writes = TokenWrites()
            let config = AppConfig(edgeURL: URL(string: "https://example.invalid")!, mode: .workos,
                userId: "user", orgId: "org", deviceId: "ios", deviceName: "test",
                tokens: AuthTokens(accessToken: expired, refreshToken: "refresh"),
                refreshTokens: { _, _ in
                    writes.save(AuthTokens(accessToken: "attempt", refreshToken: "attempt"))
                    throw AuthError.http(401, body)
                }, persistTokens: { _ in XCTFail("failed refresh persisted tokens") })
            let first = await config.currentToken()
            let second = await config.currentToken()
            XCTAssertEqual(first, terminal ? nil : expired)
            XCTAssertEqual(second, terminal ? nil : expired)
            XCTAssertEqual(writes.count, terminal ? 1 : 2)
        }
    }

    @MainActor
    func testStoppedStoreCannotBeRevivedByDeferredDial() {
        let config = AppConfig(edgeURL: URL(string: "https://example.invalid")!, mode: .dev,
            userId: "user", orgId: "org", deviceId: "ios", deviceName: "test", devBearer: "user@org")
        let id = "retirement-\(UUID().uuidString)"
        defer { try? FileManager.default.removeItem(at: DocDisk.chat2URL(for: id)) }
        let store = SessionStore(chatId: id, config: config)
        store.start(holdDial: true)
        store.updateRoomGen(2)
        store.stop()
        store.releaseDial()
        XCTAssertFalse(store.roomActive)
        config.retire()
        store.start()
        XCTAssertFalse(store.roomActive)
    }

    func testPendingMetadataSurvivesSnapshotAndLegacyHeaders() throws {
        let id = "pending-\(UUID().uuidString)"
        defer { try? FileManager.default.removeItem(at: DocDisk.chat2URL(for: id)) }
        let doc = LoroDoc()
        let command = try doc.getList(id: "commands").pushContainer(child: LoroMap())
        try command.insert(key: "status", v: "pending")
        DocDisk.saveChat2(doc: doc, id: id, cursor: 9, verified: true)
        XCTAssertTrue(DocDisk.hasPendingCommands(id: id))
        var legacy = try Data(contentsOf: DocDisk.chat2URL(for: id))
        legacy[16] = 1 // older writer knows only cursor verification
        try legacy.write(to: DocDisk.chat2URL(for: id))
        XCTAssertTrue(DocDisk.hasPendingCommands(id: id))
        try command.insert(key: "status", v: "done")
        DocDisk.saveChat2(doc: doc, id: id, cursor: 10, verified: true)
        XCTAssertFalse(DocDisk.hasPendingCommands(id: id))
    }

    @MainActor
    func testOrdinaryImageUsesDecodedBudgetAndPreservesUploadBytes() throws {
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let data = UIGraphicsImageRenderer(size: CGSize(width: 3072, height: 1024), format: format)
            .pngData { context in
                UIColor.red.setFill()
                context.fill(CGRect(x: 0, y: 0, width: 3072, height: 1024))
            }
        let loaded = try XCTUnwrap(AttachmentImageCache.decodeOrdinaryImage(data))
        let bitmap = try XCTUnwrap(loaded.image.cgImage)
        XCTAssertEqual(bitmap.width, 2048)
        XCTAssertEqual(loaded.bytes, bitmap.bytesPerRow * bitmap.height)
        XCTAssertGreaterThan(loaded.bytes, data.count)
        let staged = try XCTUnwrap(StagedAttachment.stage(data: data))
        XCTAssertEqual(staged.data, data)
        XCTAssertEqual(staged.image.cgImage?.width, 2048)
        XCTAssertNil(AttachmentImageCache.decodeOrdinaryImage(Data("bad image".utf8)))
        let cache = AttachmentImageCache()
        cache.seed(deviceId: "device", path: "/image", name: "image", data: data)
        guard case .loaded = cache.snapshot(deviceId: "device", path: "/image") else {
            return XCTFail("seeded preview missing")
        }
        cache.prune(targetBytes: 0)
        guard case .loading = cache.snapshot(deviceId: "device", path: "/image") else {
            return XCTFail("memory warning retained decoded preview")
        }
    }

    @MainActor
    func testPendingCommandsPinStoreBeforeProjectionFinishes() throws {
        let id = "pinned-\(UUID().uuidString)"
        defer { try? FileManager.default.removeItem(at: DocDisk.chat2URL(for: id)) }
        let config = AppConfig(edgeURL: URL(string: "https://example.invalid")!, mode: .dev,
            userId: "user", orgId: "org", deviceId: "ios", deviceName: "test", devBearer: "user@org")
        let store = SessionStore(chatId: id, config: config, offline: true)
        XCTAssertFalse(store.hasPendingWork)
        let command = try store.doc.getList(id: "commands").pushContainer(child: LoroMap())
        try command.insert(key: "status", v: "pending")
        XCTAssertTrue(store.hasPendingWork)
        try command.insert(key: "status", v: "done")
        XCTAssertFalse(store.hasPendingWork)
        store.stop()
    }


    @MainActor
    func testBackgroundFlushKeepsSubsequentPersistenceActive() async throws {
        let id = "flush-\(UUID().uuidString)"
        let config = AppConfig(edgeURL: URL(string: "https://example.invalid")!, mode: .dev,
            userId: "user", orgId: "org", deviceId: "ios", deviceName: "test", devBearer: "user@org")
        let store = SessionStore(chatId: id, config: config)
        store.start(holdDial: true)
        defer {
            store.stop()
            try? FileManager.default.removeItem(at: DocDisk.chat2URL(for: id))
        }
        for value in ["before background", "after foreground"] {
            try store.doc.getMap(id: "test").insert(key: "value", v: value)
            store.doc.commit()
            // Flush must persist even before the subscription hops to MainActor.
            store.flushToDisk()
            let restored = LoroDoc()
            XCTAssertNotNil(DocDisk.loadChat2(into: restored, id: id))
            XCTAssertEqual(restored.getMap(id: "test").getDeepValue().mapValue?["value"]?.stringValue, value)
        }
    }


    @MainActor
    func testChangingAccountClearsAttachmentCache() {
        let cache = AttachmentImageCache()
        func config(_ user: String) -> AppConfig {
            AppConfig(edgeURL: URL(string: "https://example.invalid")!, mode: .dev,
                userId: user, orgId: "org", deviceId: "ios", deviceName: "test", devBearer: user)
        }
        cache.configure(config: config("first"))
        let data = UIGraphicsImageRenderer(size: CGSize(width: 8, height: 8)).pngData { _ in }
        cache.seed(deviceId: "device", path: "/same/path", name: "private", data: data)
        cache.configure(config: config("second"))
        guard case .loading = cache.snapshot(deviceId: "device", path: "/same/path") else {
            return XCTFail("previous account's preview survived runtime replacement")
        }
    }

}
