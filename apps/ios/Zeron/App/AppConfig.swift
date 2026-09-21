// Session-wide identity and credentials. Retirement and credential persistence
// share one lock so an old refresh cannot write after sign-out clears Keychain.
import Foundation

final class AppConfig: @unchecked Sendable {
    enum Mode: String { case workos, dev }
    let edgeURL: URL
    let mode: Mode
    let userId: String
    let orgId: String
    let deviceId: String
    let deviceName: String

    private let lock = NSLock()
    private var tokens: AuthTokens?
    private let devBearer: String?
    private var refreshTask: Task<String?, Never>?
    private var retired = false
    private var needsReauthentication = false
    private let refreshTokens: @Sendable (String, String) async throws -> AuthTokens
    private let persistTokens: @Sendable (AuthTokens) -> Void
    @MainActor var onTerminalAuthError: (() -> Void)?

    init(edgeURL: URL, mode: Mode, userId: String, orgId: String,
         deviceId: String, deviceName: String,
         tokens: AuthTokens? = nil, devBearer: String? = nil,
         refreshTokens: (@Sendable (String, String) async throws -> AuthTokens)? = nil,
         persistTokens: @escaping @Sendable (AuthTokens) -> Void = {
             Keychain.save($0.accessToken, key: "accessToken")
             Keychain.save($0.refreshToken, key: "refreshToken")
         }) {
        self.edgeURL = edgeURL
        self.mode = mode
        self.userId = userId
        self.orgId = orgId
        self.deviceId = deviceId
        self.deviceName = deviceName
        self.tokens = tokens
        self.devBearer = devBearer
        self.refreshTokens = refreshTokens ?? { token, org in
            try await AuthClient(baseURL: edgeURL).refresh(refreshToken: token, organizationId: org)
        }
        self.persistTokens = persistTokens
    }

    var isRetired: Bool { lock.withLock { retired } }

    func retire() {
        let task = lock.withLock {
            retired = true
            tokens = nil
            let task = refreshTask
            refreshTask = nil
            return task
        }
        task?.cancel()
    }

    func currentToken() async -> String? {
        let available = lock.withLock { !retired && !needsReauthentication }
        guard available else { return nil }
        if mode == .dev { return lock.withLock { retired ? nil : devBearer } }
        guard let current = lock.withLock({ tokens }) else { return nil }
        if !Self.isExpired(jwt: current.accessToken) {
            return lock.withLock { retired || needsReauthentication ? nil : current.accessToken }
        }
        return await refreshedToken()
    }

    private func refreshedToken() async -> String? {
        let task: Task<String?, Never>? = lock.withLock {
            guard !retired, !needsReauthentication, let current = tokens else { return nil }
            if let existing = refreshTask { return existing }
            // A caller that waited for the lock may already have fresh tokens.
            if !Self.isExpired(jwt: current.accessToken) { return Task { current.accessToken } }
            let task = Task<String?, Never> { [self] in
                do {
                    let refreshed = try await refreshTokens(current.refreshToken, orgId)
                    return lock.withLock {
                        guard !retired, !Task.isCancelled else { return nil }
                        tokens = refreshed
                        // Keep the write inside the retirement boundary. retire()
                        // cannot return until these writes have completed.
                        persistTokens(refreshed)
                        refreshTask = nil
                        return refreshed.accessToken
                    }
                } catch {
                    let terminal = (error as? AuthError)?.isTerminal ?? false
                    let fallback: String? = lock.withLock {
                        guard !retired, !Task.isCancelled else { return nil }
                        refreshTask = nil
                        if terminal { needsReauthentication = true }
                        return terminal ? nil : current.accessToken
                    }
                    if terminal {
                        await MainActor.run {
                            guard !self.isRetired else { return }
                            self.onTerminalAuthError?()
                        }
                    }
                    return fallback
                }
            }
            refreshTask = task
            return task
        }
        return await task?.value
    }

    private var wsBase: URL {
        var components = URLComponents(url: edgeURL, resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        return components.url!
    }

    /// The workspace registry room (docs/registry-sync.md) — the row-table
    /// replacement for the old ws Loro workspace doc.
    func registrySocketURL() async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "registry/\(orgId)/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token),
                                URLQueryItem(name: "device", value: deviceId)])
        return url
    }

    /// The chat2 log-relay room (docs/chat2-sync.md B) — replaces the s2
    /// session rooms, which mobile no longer dials at all. `device` rides the
    /// URL so the DO can attribute sockets and honor excludeOwn backfills.
    func chat2SocketURL(chatId: String) async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "chat2/\(chatId)/ws")
        url.append(queryItems: [URLQueryItem(name: "token", value: token),
                                URLQueryItem(name: "device", value: deviceId)])
        return url
    }

    /// GET /chat2/{chatId}/checkpoint — the Range-resumable doc snapshot
    /// (auth via bearer header; the caller adds Range on resume).
    func chat2CheckpointRequest(chatId: String) async -> URLRequest? {
        guard let token = await currentToken() else { return nil }
        var request = URLRequest(url: edgeURL.appending(path: "chat2/\(chatId)/checkpoint"))
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// GET /chat2/{chatId}/rows?after= — pull over plain HTTPS: one request
    /// collapses the socket's connect→hello→state→rowsReq→backfill, and it
    /// works on networks that strip WS upgrades (airplane wifi).
    func chat2RowsRequest(chatId: String, after: UInt64) async -> URLRequest? {
        guard let token = await currentToken() else { return nil }
        var url = edgeURL.appending(path: "chat2/\(chatId)/rows")
        url.append(queryItems: [URLQueryItem(name: "after", value: String(after)),
                                URLQueryItem(name: "device", value: deviceId)])
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// POST /chat2/{chatId}/rows?batchId= — push over plain HTTPS (batchId
    /// dedupe makes replays no-ops); body is the raw update batch.
    func chat2PushRequest(chatId: String, batchId: String) async -> URLRequest? {
        guard let token = await currentToken() else { return nil }
        var url = edgeURL.appending(path: "chat2/\(chatId)/rows")
        url.append(queryItems: [URLQueryItem(name: "batchId", value: batchId),
                                URLQueryItem(name: "device", value: deviceId)])
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// GET /registry/{orgId}/rows?since= — the WS hello's delta answer over
    /// plain HTTPS. `beat=1` doubles as a presence beat.
    func registryRowsRequest(since: UInt64?) async -> URLRequest? {
        guard let token = await currentToken() else { return nil }
        var url = edgeURL.appending(path: "registry/\(orgId)/rows")
        var items = [URLQueryItem(name: "device", value: deviceId),
                     URLQueryItem(name: "beat", value: "1")]
        if let since { items.append(URLQueryItem(name: "since", value: String(since))) }
        url.append(queryItems: items)
        // Bearer header, never ?token=: HTTP supports headers (unlike WS
        // upgrades), and query strings can reach request logs.
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// POST /registry/{orgId}/push — one op batch over plain HTTPS (LWW
    /// clocks make replays apply zero ops).
    func registryPushRequest() async -> URLRequest? {
        guard let token = await currentToken() else { return nil }
        var url = edgeURL.appending(path: "registry/\(orgId)/push")
        url.append(queryItems: [URLQueryItem(name: "device", value: deviceId)])
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        return request
    }

    /// Decode the JWT payload's `exp` (60s early-refresh margin). Unparseable
    /// tokens read as non-expired — the server is the arbiter.
    private static func isExpired(jwt: String) -> Bool {
        let segments = jwt.split(separator: ".")
        guard segments.count == 3 else { return false }
        var base64 = String(segments[1]).replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while base64.count % 4 != 0 { base64 += "=" }
        guard let data = Data(base64Encoded: base64),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let exp = obj["exp"] as? TimeInterval else { return false }
        return Date().timeIntervalSince1970 > exp - 60
    }

    /// GET /device/{deviceId}/status → whether the device's relay HOST socket
    /// is currently attached (distinct from workspace presence).
    func deviceStatus(deviceId: String) async -> String {
        guard let token = await currentToken() else { return "no-token" }
        var request = URLRequest(url: edgeURL.appending(path: "device/\(deviceId)/status"))
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        guard let (data, response) = try? await URLSession.shared.data(for: request),
              let http = response as? HTTPURLResponse else { return "unreachable" }
        return "http=\(http.statusCode) body=\(String(data: data, encoding: .utf8) ?? "")"
    }

    /// POST /device/{deviceId}/nudge {chatId} — wake a cold host to drain the
    /// command queue.
    func nudge(deviceId: String, chatId: String) async {
        guard let token = await currentToken() else { return }
        var request = URLRequest(url: edgeURL.appending(path: "device/\(deviceId)/nudge"))
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["chatId": chatId])
        _ = try? await URLSession.shared.data(for: request)
    }
}
