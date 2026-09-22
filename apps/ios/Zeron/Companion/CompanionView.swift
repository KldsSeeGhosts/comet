import SwiftUI
import VisionKit

struct CompanionView: View {
    @Environment(AppModel.self) private var app
    @Environment(\.scenePhase) private var scenePhase
    @State private var model = CompanionModel()
    @State private var pairing = false
    @State private var settings = false
    @State private var newSession = false
    @State private var cloud = false
    @State private var path: [HostChat] = []
    @State private var initialProject = ""

    var body: some View {
        NavigationStack(path: $path) {
            Group {
                if model.selected != nil {
                    CompanionDashboard(model: model, newSession: { initialProject = $0; newSession = true }, pair: { pairing = true }, opened: { path.append($0) })
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 28) { heading; welcome }
                            .padding(24).frame(maxWidth: 620).frame(maxWidth: .infinity)
                    }
                }
            }
            .background(Theme.bg.ignoresSafeArea())
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    HStack(spacing: 9) {
                        Image("zeron-logo").resizable().scaledToFit().frame(width: 16, height: 19)
                        Text("noches").font(Theme.sans(20, weight: .semibold)).tracking(-0.7)
                    }.foregroundStyle(Theme.text).fixedSize()
                }.sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarTrailing) {
                    Button { settings = true } label: { Image(systemName: "ellipsis") }
                        .accessibilityLabel("Appearance and computers")
                }
            }
            .navigationDestination(for: HostChat.self) { chat in
                CompanionSessionView(model: model, chat: chat)
            }
            .sheet(isPresented: $pairing) { PairComputerSheet(model: model) }
            .sheet(isPresented: $newSession) {
                NewHostSessionSheet(model: model, initialProject: initialProject) { path.append($0) }
            }
            .sheet(isPresented: $cloud) { SignInView() }
            .sheet(isPresented: $settings) {
                NavigationStack {
                    List {
                        NavigationLink("Appearance") { AppearanceSettingsView() }
                        Section("Computers") {
                            ForEach(model.profiles) { host in
                                Button { model.select(host.id); settings = false } label: {
                                    HStack {
                                        Label(host.name, systemImage: "desktopcomputer")
                                        Spacer()
                                        if model.selectedID == host.id { Image(systemName: "checkmark") }
                                    }
                                }
                                .swipeActions {
                                    Button("Forget", role: .destructive) {
                                        do { try model.forget(host) } catch { model.error = error.localizedDescription }
                                    }
                                }
                            }
                            Button("Pair a computer") { settings = false; pairing = true }
                        }
                        Section {
                            Button("Connect a cloud account") { settings = false; cloud = true }
                            Button("Explore demo sessions") { settings = false; app.enterDemoMode() }
                        }
                        Text("Forgetting a computer removes its key from this phone. Revoke the key on the host to disable it everywhere.")
                            .font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
                    }
                    .scrollContentBackground(.hidden).background(Theme.bg)
                    .navigationTitle("Settings")
                    .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { settings = false } } }
                }
            }
        }
        .tint(Theme.text)
        .task(id: "\(model.selectedID ?? "")-\(model.connectionRevision)-\(scenePhase == .active)") {
            guard scenePhase == .active else { model.connection.close(); model.online = false; return }
            await model.maintainConnection()
        }
        .onAppear {
            if ProcessInfo.processInfo.arguments.contains("-companion-settings") { settings = true }
        }
        .onChange(of: model.selectedID) { _, _ in path = [] }
    }

    private var heading: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(model.selected == nil ? "Your computer.\nWithin reach." : "Your computers")
                .font(Theme.sans(34, weight: .semibold)).tracking(-1.4)
                .foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true)
            Text(model.selected == nil ? "Pair your Mac or Linux host to pick up a session and put your agents to work." : "The work stays on your computer. You stay in control.")
                .font(Theme.sans(15)).foregroundStyle(Theme.textMuted).lineSpacing(4)
        }.padding(.top, 16)
    }

    private var welcome: some View {
        VStack(alignment: .leading, spacing: 24) {
            HStack(spacing: 20) {
                Image(systemName: "desktopcomputer").font(.system(size: 54, weight: .ultraLight))
                Image(systemName: "link").font(.system(size: 17)).foregroundStyle(Theme.accent)
                Image(systemName: "iphone").font(.system(size: 44, weight: .ultraLight))
            }.foregroundStyle(Theme.text).frame(maxWidth: .infinity).padding(.vertical, 28).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 8) {
                Text("Start with a computer").font(Theme.sans(21, weight: .medium))
                Text("Connect to a running Noches instance. Your files, agent accounts, and sessions stay on the host.")
                    .font(Theme.sans(15)).foregroundStyle(Theme.textMuted).lineSpacing(4)
            }
            Button { pairing = true } label: {
                HStack { Text("Pair a computer"); Spacer(); Image(systemName: "arrow.right") }
                    .font(Theme.sans(16, weight: .medium)).padding(18)
                    .background(Theme.text, in: Capsule()).foregroundStyle(Theme.bg)
            }.accessibilityIdentifier("pair-computer")
            Text("macOS & Linux · Private connection").font(Theme.mono(11)).foregroundStyle(Theme.textFaint)
                .frame(maxWidth: .infinity)
            if let error = model.error { Text(error).font(Theme.sans(13)).foregroundStyle(Theme.danger) }
        }
        .foregroundStyle(Theme.text).padding(24).modifier(CompanionPanel())
    }

}

func statusLabel(_ status: String) -> String {
    switch status {
    case "working": return "Working"
    case "awaitingInput": return "Needs your input"
    case "errored": return "Needs attention"
    case "completed": return "Done"
    default: return "Ready"
    }
}

struct PairComputerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let model: CompanionModel
    @State private var code = ""
    @State private var scanning = false
    @State private var error: String?
    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    Image(systemName: "link").font(.system(size: 36, weight: .light)).foregroundStyle(Theme.accent)
                    Text("Bring your computer along.").font(Theme.sans(29, weight: .semibold)).tracking(-0.8)
                    Text("Paste the private connection code created by the Noches connection gateway on your Mac or Linux host.")
                        .font(Theme.sans(16)).foregroundStyle(Theme.textMuted)
                    if DataScannerViewController.isSupported {
                        Button { scanning = true } label: {
                            Label("Scan connection code", systemImage: "qrcode.viewfinder")
                                .font(Theme.sans(15, weight: .medium)).frame(maxWidth: .infinity, minHeight: 52)
                                .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 16))
                        }
                    }
                    SecureField("noches-connect:…", text: $code)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .font(Theme.mono(13)).padding(18).background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 16))
                        .accessibilityIdentifier("connection-code")
                    Button {
                        do { try model.pair(code); code = ""; dismiss() } catch { self.error = error.localizedDescription }
                    } label: {
                        Text("Connect to computer").font(Theme.sans(16, weight: .medium)).frame(maxWidth: .infinity).padding(18)
                            .background(Theme.text, in: Capsule()).foregroundStyle(Theme.bg)
                    }.disabled(code.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    if let error { Text(error).foregroundStyle(Theme.danger).font(Theme.sans(14)) }
                    Label("The code grants access to this computer. It is stored securely in your phone's Keychain.", systemImage: "lock")
                        .font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
                    VStack(alignment: .leading, spacing: 12) {
                        Label("Connect Tailscale on both devices", systemImage: "network")
                        Label("Keep Noches running on your computer", systemImage: "desktopcomputer")
                        Label("Connect from Wi-Fi or cellular", systemImage: "antenna.radiowaves.left.and.right")
                    }.font(Theme.sans(13)).foregroundStyle(Theme.textMuted).padding(.vertical, 8)
                    DisclosureGroup("Set up your host") {
                        Text("Run Noches on your computer, then use noches-connect pair to create a phone key and noches-connect serve to enable the connection. Use your host's Tailscale IP address and keep Tailscale connected on both devices. The phone cannot wake or start a stopped host service.")
                            .font(Theme.sans(14)).foregroundStyle(Theme.textMuted).padding(.top, 10)
                    }.font(Theme.sans(14))
                }.padding(24).frame(maxWidth: 640)
            }.background(Theme.bg).foregroundStyle(Theme.text)
                .sheet(isPresented: $scanning) {
                    NavigationStack {
                        CompanionScanner(scanned: { code = $0; scanning = false }, failed: { error = $0; scanning = false })
                            .ignoresSafeArea(edges: .bottom)
                            .navigationTitle("Scan connection code").navigationBarTitleDisplayMode(.inline)
                            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { scanning = false } } }
                    }
                }
                .navigationTitle("Pair a computer").navigationBarTitleDisplayMode(.inline)
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        }
    }
}

private struct NewHostSessionSheet: View {
    @Environment(\.dismiss) private var dismiss
    let model: CompanionModel
    var initialProject = ""
    let opened: (HostChat) -> Void
    @State private var space = ""
    @State private var harness = ""
    @State private var agentModel = ""
    @State private var reasoning = ""
    @State private var models: [HostAgentModel] = []
    @State private var loadingModels = false
    @State private var catalogError: String?
    @State private var busy = false
    @State private var error: String?
    var body: some View {
        NavigationStack {
            Form {
                Section("Run on") { Label(model.selected?.name ?? "Computer", systemImage: "desktopcomputer") }
                Section {
                    Picker("Project", selection: $space) {
                        Text("No project · home folder").tag("")
                        ForEach(model.localSpaces) { Text($0.displayName).tag($0.id) }
                    }
                    Picker("Agent", selection: $harness) {
                        ForEach(model.harnesses) { Text($0.label).tag($0.id) }
                    }
                }
                Section("Model") {
                    Picker("Model", selection: $agentModel) {
                        Text("Agent default").tag("")
                        ForEach(models) { Text($0.label).tag($0.id) }
                    }.disabled(loadingModels)
                    if let chosen = models.first(where: { $0.id == agentModel }), !chosen.reasoningLevels.isEmpty {
                        Picker("Reasoning", selection: $reasoning) {
                            Text("Default").tag("")
                            ForEach(chosen.reasoningLevels, id: \.self) { Text($0.capitalized).tag($0) }
                        }
                    }
                    if loadingModels { ProgressView("Loading models from your computer…") }
                    if let catalogError { Text(catalogError).font(Theme.sans(12)).foregroundStyle(Theme.textMuted) }
                }
                Section {
                    Button(busy ? "Creating…" : "Create session") {
                        busy = true
                        Task {
                            do {
                                let chat = try await model.create(spaceID: space.isEmpty ? nil : space, harness: harness, model: agentModel.isEmpty ? nil : agentModel, reasoning: reasoning.isEmpty ? nil : reasoning)
                                dismiss(); opened(chat)
                            } catch { self.error = error.localizedDescription }
                            busy = false
                        }
                    }.disabled(busy || harness.isEmpty || !model.online)
                    if let error { Text(error).foregroundStyle(Theme.danger) }
                } footer: { Text("New sessions allow workspace writes and ask for approval when required.") }
            }.scrollContentBackground(.hidden).background(Theme.bg)
                .navigationTitle("New session").navigationBarTitleDisplayMode(.inline)
                .onAppear { harness = model.harnesses.first?.id ?? ""; space = initialProject }
                .task(id: harness) {
                    agentModel = ""; reasoning = ""; models = []; catalogError = nil
                    guard !harness.isEmpty else { return }
                    loadingModels = true
                    do {
                        let catalog = try await model.models(for: harness)
                        guard !Task.isCancelled else { return }
                        models = catalog
                    } catch {
                        guard !Task.isCancelled else { return }
                        catalogError = "Couldn't load models. You can still use the agent's default."
                    }
                    loadingModels = false
                }
                .onChange(of: agentModel) { _, _ in reasoning = "" }
                .onChange(of: model.harnesses.map(\.id)) { _, ids in if harness.isEmpty { harness = ids.first ?? "" } }
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        }
    }
}

struct CompanionSessionView: View {
    let model: CompanionModel
    let chat: HostChat
    @State private var messages: [HostMessage] = []
    @Environment(\.dismiss) private var dismiss
    private var draft: Binding<String> {
        Binding(get: { model.drafts[model.draftKey(for: chat)] ?? "" },
                set: { model.drafts[model.draftKey(for: chat)] = $0 })
    }
    @State private var error: String?
    @State private var busy = false
    @State private var loaded = false
    @State private var queue: [JSONValue] = []
    @State private var scroll = ScrollState()
    @State private var submittedID: String?
    @State private var inspector: CompanionInspector?
    @FocusState private var composerFocused: Bool
    private var currentChat: HostChat { model.chats.first { $0.id == chat.id } ?? chat }

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                CompanionTranscript(messages: messages, scroll: scroll, online: model.online, busy: busy, submittedID: submittedID) { id, answers in
                    perform { try await model.respond(requestID: id, answers: answers, chat: currentChat) }
                }
                if !loaded { ProgressView("Loading session…").font(Theme.sans(13)) }
                if loaded && messages.isEmpty {
                    VStack(spacing: 14) {
                        CompanionAvatar(status: "idle", seed: chat.id).scaleEffect(1.3)
                        Text("What are we working on?").font(Theme.sans(21, weight: .medium)).tracking(-0.5)
                        Text(currentChat.cwd ?? "Home folder").font(Theme.mono(11)).foregroundStyle(Theme.textMuted)
                    }.padding(30)
                }
            }
            composer
        }
        .background(Theme.bg).foregroundStyle(Theme.text)
        .navigationTitle(currentChat.displayTitle).navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button { inspector = .files } label: { Image(systemName: "folder") }.accessibilityLabel("Browse files")
                Button { inspector = .changes } label: { Image(systemName: "arrow.triangle.branch") }.accessibilityLabel("Review changes")
                CompanionSessionMenu(model: model, chat: currentChat, archived: { dismiss() })
            }
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button("Done") { composerFocused = false }
            }
        }
        .sheet(item: $inspector) { CompanionWorkspaceSheet(model: model, chat: currentChat, tab: $0) }
        .task(id: "\(model.generation)-\(model.online)") { await watchTranscript() }
        .task(id: "queue-\(model.generation)-\(model.online)") {
            guard model.online else { return }
            let connection = model.connection
            do {
                for try await value in try await connection.watch("WatchQueue", ["chatId": chat.id]) {
                    guard !Task.isCancelled, connection === model.connection else { return }
                    queue = value.objectValue?["items"]?.arrayValue ?? []
                }
            } catch { if !Task.isCancelled { self.error = error.localizedDescription } }
        }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let error { Text(error).font(Theme.sans(12)).foregroundStyle(Theme.danger).textSelection(.enabled).padding(.horizontal, 8) }
            if !model.online { Text("Reconnecting to your computer…").font(Theme.sans(12)).foregroundStyle(Theme.warning).padding(.horizontal, 8) }
            if !queue.isEmpty {
                DisclosureGroup("Queued · \(queue.count)") {
                    ForEach(Array(queue.enumerated()), id: \.offset) { _, row in
                        Text(row.objectValue?["text"]?.stringValue ?? "Queued message").font(Theme.sans(13))
                            .foregroundStyle(Theme.textMuted).frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 6)
                    }
                }.font(Theme.sans(12)).padding(.horizontal, 12)
            }
            HStack(spacing: 6) {
                if let harness = currentChat.config?.harness {
                    BrandMarkShape(mark: .forHarness(harness)).fill(BrandMark.tint(for: harness))
                        .frame(width: 11, height: 11)
                    Text(currentChat.config?.model ?? HarnessCatalog.label(for: harness)).font(Theme.sans(11)).lineLimit(1)
                }
                Spacer()
                if ["working", "awaitingInput"].contains(model.status(chat.id)) {
                    Text(statusLabel(model.status(chat.id))).font(Theme.sans(11)).foregroundStyle(companionStatusColor(model.status(chat.id)))
                    Button { perform { try await model.stop(currentChat) } } label: {
                        Image(systemName: "stop.fill").font(.system(size: 10)).frame(width: 32, height: 32)
                    }.accessibilityLabel("Stop session").disabled(busy || !model.online)
                } else {
                    Text(currentChat.branch ?? "").font(Theme.mono(10)).lineLimit(1)
                }
            }.foregroundStyle(Theme.textMuted).padding(.horizontal, 14).frame(minHeight: 26)
            HStack(alignment: .bottom, spacing: 10) {
                TextField(["working", "awaitingInput"].contains(model.status(chat.id)) ? "Queue a follow-up…" : "Do anything…", text: draft, axis: .vertical).lineLimit(1...6)
                    .font(Theme.sans(16)).focused($composerFocused).accessibilityIdentifier("companion-composer")
                    .padding(.vertical, 11).padding(.leading, 8)
                Button {
                    let text = draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines)
                    let id = UUID().uuidString.lowercased()
                    let queued = ["working", "awaitingInput"].contains(model.status(chat.id))
                    scroll.arm()
                    perform {
                        try await model.send(text, chat: currentChat, messageID: id)
                        if !queued { submittedID = id }
                        if draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines) == text { draft.wrappedValue = "" }
                    }
                } label: {
                    Image(systemName: "arrow.up").font(.system(size: 16, weight: .medium)).frame(width: 38, height: 38)
                        .background(Theme.text.opacity(canSend ? 1 : 0.12), in: Circle()).foregroundStyle(canSend ? Theme.bg : Theme.textMuted)
                        .frame(width: 44, height: 44)
                }.accessibilityLabel("Send message").disabled(!canSend)
            }
            .padding(6).padding(.leading, 4).nochesGlass(in: RoundedRectangle(cornerRadius: 28))
        }.padding(.horizontal, 12).padding(.top, 6).padding(.bottom, 8)
            .background(Theme.bg)
    }
    private var canSend: Bool { !busy && loaded && model.online && !draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }

    private func perform(_ action: @escaping () async throws -> Void) {
        busy = true; error = nil
        Task {
            do { try await action() } catch { self.error = error.localizedDescription }
            busy = false
        }
    }
    private func watchTranscript() async {
        guard model.online else { return }
        loaded = false
        let connection = model.connection
        while !Task.isCancelled && model.online {
            do {
                for try await value in try await connection.watch("WatchDocMessages", ["chatId": chat.id]) {
                    guard !Task.isCancelled, model.connection === connection else { return }
                    let frame = try CompanionModel.decode(HostTranscriptFrame.self, value)
                    messages = try frame.applying(to: messages)
                    loaded = true
                }
                return
            } catch {
                if Task.isCancelled { return }
                self.error = error.localizedDescription
                do { try await Task.sleep(for: .seconds(2)) } catch { return }
            }
        }
    }
}
