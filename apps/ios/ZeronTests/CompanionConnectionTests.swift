import XCTest
@testable import Zeron

final class CompanionConnectionTests: XCTestCase {
    private func profile(_ endpoint: String) -> ConnectionProfile {
        ConnectionProfile(id: "phone", name: "Studio", endpoint: endpoint, token: String(repeating: "a", count: 64), deviceId: "mac")
    }
    func testEncryptedAndExactPrivateRanges() throws {
        for endpoint in ["ws://100.64.0.0:1234", "ws://100.127.255.255:1234", "ws://127.0.0.1:2", "ws://[::1]:2", "ws://[fd7a:115c:a1e0::1]:2", "wss://host.example"] {
            XCTAssertNoThrow(try profile(endpoint).validate(), endpoint)
        }
        for endpoint in ["ws://100.63.255.255", "ws://100.128.0.0", "ws://192.168.1.2", "ws://localhost", "ws://host.example", "ws://[fd7a:115c:a1e1::1]", "https://host.example", "wss://user:key@host.example", "wss://host.example/path", "wss://host.example?key=x", "wss://host.example#x", "ws://127.0.0.1:0"] {
            XCTAssertThrowsError(try profile(endpoint).validate(), endpoint)
        }
    }
    func testCodeRoundTripAndInvalidSecret() throws {
        let expected = profile("ws://100.114.177.75:27657")
        let encoded = try JSONEncoder().encode(expected).base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
        XCTAssertEqual(try ConnectionProfile.parse("  noches-connect:" + encoded + "\n"), expected)
        for code in ["not-a-code", "noches-connect:%%%", String(repeating: "x", count: 8193)] { XCTAssertThrowsError(try ConnectionProfile.parse(code)) }
        let invalid = ConnectionProfile(id: "p", name: "Studio", endpoint: "wss://host.example", token: String(repeating: "z", count: 64), deviceId: "d")
        XCTAssertThrowsError(try invalid.validate())
    }
    private func frame(_ text: String) throws -> HostTranscriptFrame { try JSONDecoder().decode(HostTranscriptFrame.self, from: Data(text.utf8)) }
    func testTranscriptDeltasAndUnicodeLengths() throws {
        let initial = try frame(#"{"reset":[{"id":"a","role":"assistant","parts":[{"id":"p","kind":"text","text":"Hi"}]}]}"#).applying(to: [])
        let appended = try frame(#"{"append":[{"entry":"a","part":"p","text":" 🌙","len":7}],"count":1}"#).applying(to: initial)
        XCTAssertEqual(appended.first?.parts.first?.text, "Hi 🌙")
        let inserted = try frame(#"{"upsert":[{"after":"a","entry":{"id":"b","role":"user","parts":[]}}],"count":2}"#).applying(to: appended)
        XCTAssertEqual(inserted.map(\.id), ["a", "b"])
        let removed = try frame(#"{"remove":["a"],"count":1}"#).applying(to: inserted)
        XCTAssertEqual(removed.map(\.id), ["b"])
        for invalid in [#"{"append":[{"entry":"a","part":"p","text":"🌙","len":3}],"count":1}"#,
                        #"{"upsert":[{"after":"missing","entry":{"id":"b","role":"user","parts":[]}}],"count":2}"#,
                        #"{"count":9}"#] { XCTAssertThrowsError(try frame(invalid).applying(to: initial)) }
    }
    func testHostCatalogAndInputUseRustFieldNames() throws {
        let data = Data(#"{"id":"codex","name":"Codex","installed":true,"enabled":true}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(HostHarness.self, from: data).label, "Codex")
        let input = Data(#"{"id":"part-id","kind":"input","requestId":"request-id","questions":[],"resolved":false}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(HostPart.self, from: input).requestId, "request-id")
    }
}
