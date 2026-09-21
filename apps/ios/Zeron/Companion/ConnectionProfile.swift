import Foundation
import Darwin
import Security

/// Wire-compatible with crates/rpc/src/remote/config.rs.
struct ConnectionProfile: Codable, Identifiable, Equatable {
    let id: String
    let name: String
    let endpoint: String
    let token: String
    let deviceId: String

    static func parse(_ code: String) throws -> Self {
        let code = code.trimmingCharacters(in: .whitespacesAndNewlines)
        guard code.utf8.count <= 8192, code.hasPrefix("noches-connect:") else {
            throw RelayError.rpc("Paste the connection code from your computer.")
        }
        var encoded = String(code.dropFirst("noches-connect:".count))
            .replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        encoded += String(repeating: "=", count: (4 - encoded.count % 4) % 4)
        guard let data = Data(base64Encoded: encoded),
              let profile = try? JSONDecoder().decode(Self.self, from: data) else {
            throw RelayError.rpc("This connection code is incomplete or invalid.")
        }
        try profile.validate()
        return profile
    }

    func validate() throws {
        guard !id.isEmpty, !deviceId.isEmpty,
              !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, name.utf8.count <= 128,
              token.utf8.count == 64, token.utf8.allSatisfy({ (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) }) else {
            throw RelayError.rpc("The connection code has an invalid computer identity or key.")
        }
        guard let url = URLComponents(string: endpoint), let host = url.host,
              url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
              url.path.isEmpty || url.path == "/", url.url != nil,
              url.port == nil || (1...65535).contains(url.port!) else {
            throw RelayError.rpc("Use a server address without a path, credentials, or query.")
        }
        guard url.scheme == "wss" || (url.scheme == "ws" && Self.isPrivateAddress(host)) else {
            throw RelayError.rpc("Direct connections need a Tailscale IP address or a secure wss:// server.")
        }
    }

    static func isPrivateAddress(_ raw: String) -> Bool {
        let host = raw.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        var v4 = in_addr()
        if inet_pton(AF_INET, host, &v4) == 1 {
            let n = UInt32(bigEndian: v4.s_addr)
            return n >> 24 == 127 || n & 0xffc00000 == 0x64400000
        }
        var v6 = in6_addr()
        if inet_pton(AF_INET6, host, &v6) == 1 {
            return withUnsafeBytes(of: &v6) { bytes in
                (bytes.prefix(15).allSatisfy { $0 == 0 } && bytes[15] == 1)
                    || Array(bytes.prefix(6)) == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0]
            }
        }
        return false
    }
}

/// Pairing keys stay on this phone and are never stored in preferences or logs.
enum CompanionKeychain {
    private static var query: [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: "noches.companion", kSecAttrAccount as String: "computers"]
    }

    static func load() throws -> [ConnectionProfile] {
        var q = query
        q[kSecReturnData as String] = true
        var result: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &result)
        if status == errSecItemNotFound { return [] }
        guard status == errSecSuccess, let data = result as? Data else {
            throw RelayError.rpc("Unlock your phone to access paired computers.")
        }
        let profiles = try JSONDecoder().decode([ConnectionProfile].self, from: data)
        try profiles.forEach { try $0.validate() }
        return profiles
    }

    static func save(_ profiles: [ConnectionProfile]) throws {
        let data = try JSONEncoder().encode(profiles)
        let values: [String: Any] = [kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly]
        var status = SecItemUpdate(query as CFDictionary, values as CFDictionary)
        if status == errSecItemNotFound {
            status = SecItemAdd(query.merging(values) { _, new in new } as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw RelayError.rpc("Could not save this computer in Keychain.") }
    }
}
