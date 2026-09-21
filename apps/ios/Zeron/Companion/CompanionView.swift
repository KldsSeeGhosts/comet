import SwiftUI

struct CompanionView: View {
    @Environment(AppModel.self) private var app
    @Environment(\.scenePhase) private var scenePhase
    @State private var model = CompanionModel()
    @State private var pairing = false
    @State private var settings = false
    @State private var newSession = false
    @State private var cloud = false
    @State private var path: [HostChat] = []
    @State private var filter = ""

    var body: some View {
        NavigationStack(path: $path) {
            ScrollView {
                VStack(alignment: .leading, spacing: 28) {
                    heading
                    if let host = model.selected {
                        hostCard(host)
                        sessionList
                    } else { welcome }
                }
                .padding(24)
                .frame(maxWidth: 720)
                .frame(maxWidth: .infinity)
            }
            .background(Theme.bg.ignoresSafeArea())
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Text("noches").font(Theme.sans(20, weight: .semibold)).tracking(-0.7).fixedSize()
                }.sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarTrailing) {
                    Button { settings = true } label: { Image(systemName: "slider.horizontal.3") }
                        .accessibilityLabel("Appearance and computers")
                }
            }
            .navigationDestination(for: HostChat.self) { chat in
                CompanionSessionView(model: model, chat: chat)
            }
            .sheet(isPresented: $pairing) { PairComputerSheet(model: model) }
            .sheet(isPresented: $newSession) {
                NewHostSessionSheet(model: model) { path.append($0) }
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
        .task(id: "\(model.selectedID ?? "")-\(scenePhase == .active)") {
            guard scenePhase == .active else { model.connection.close(); model.online = false; return }
            await model.maintainConnection()
        }
        .onAppear {
            if ProcessInfo.processInfo.arguments.contains("-companion-settings") { settings = true }
        }
        .onChange(of: model.selectedID) { _, _ in path = []; filter = "" }
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

    private func hostCard(_ host: ConnectionProfile) -> some View {
        VStack(alignment: .leading, spacing: 24) {
            HStack(alignment: .top) {
                Image(systemName: "desktopcomputer").font(.system(size: 40, weight: .ultraLight))
                    .foregroundStyle(Theme.text).accessibilityHidden(true)
                Spacer()
                HStack(spacing: 6) {
                    Circle().fill(model.online ? Theme.statusCompleted : Theme.warning).frame(width: 6, height: 6)
                    Text(model.connectionMessage).font(Theme.mono(11))
                }.foregroundStyle(Theme.textMuted)
            }
            VStack(alignment: .leading, spacing: 6) {
                Text(host.name).font(Theme.sans(24, weight: .medium)).foregroundStyle(Theme.text)
                Text(URL(string: host.endpoint)?.host ?? "Paired computer")
                    .font(Theme.mono(12)).foregroundStyle(Theme.textMuted)
            }
            HStack {
                Text("\(model.localChats.count) \(model.localChats.count == 1 ? "session" : "sessions")").font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
                Spacer()
                Button { newSession = true } label: { Label("New session", systemImage: "plus") }
                    .font(Theme.sans(14, weight: .medium)).disabled(!model.online)
            }
            if !model.online {
                Text("Keep Noches running on this host and connect your phone to the same Tailscale network.")
                    .font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
                if let error = model.error { Text(error).font(Theme.sans(12)).foregroundStyle(Theme.warning) }
            }
        }
        .padding(24).modifier(CompanionPanel())
        .overlay(RoundedRectangle(cornerRadius: 24).stroke(Theme.border, lineWidth: 1))
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

    private var sessionList: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                Text("Sessions").font(Theme.sans(21, weight: .semibold))
                Spacer()
                Menu {
                    Button("All projects") { filter = "" }
                    ForEach(model.localSpaces) { space in Button(space.displayName) { filter = space.id } }
                } label: {
                    Label(model.localSpaces.first { $0.id == filter }?.displayName ?? "All projects", systemImage: "line.3.horizontal.decrease")
                        .font(Theme.sans(12)).foregroundStyle(Theme.textMuted)
                }
            }.foregroundStyle(Theme.text)
            let chats = model.localChats.filter { filter.isEmpty || $0.spaceId == filter }
            if chats.isEmpty {
                Text(model.online ? "Start a session on this computer. It will appear here and on your desktop." : "Sessions will appear when your computer connects.")
                    .font(Theme.sans(15)).foregroundStyle(Theme.textMuted).padding(.vertical, 12)
            }
            ForEach(chats) { chat in
                NavigationLink(value: chat) {
                    HStack(alignment: .top, spacing: 14) {
                        Image(systemName: model.status(chat.id) == "working" ? "waveform" : "bubble.left.and.text.bubble.right")
                            .font(.system(size: 18)).foregroundStyle(model.status(chat.id) == "working" ? Theme.accent : Theme.textMuted)
                            .frame(width: 44, height: 44).background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 15))
                        VStack(alignment: .leading, spacing: 7) {
                            Text(chat.displayTitle).font(Theme.sans(16, weight: .medium)).foregroundStyle(Theme.text).lineLimit(2)
                            Text(chat.lastMessagePreview ?? chat.cwd ?? "No project").font(Theme.sans(13)).foregroundStyle(Theme.textMuted).lineLimit(1)
                            Text(statusLabel(model.status(chat.id))).font(Theme.mono(10)).foregroundStyle(Theme.textFaint)
                        }
                        Spacer(minLength: 0)
                        Image(systemName: "chevron.right").font(.system(size: 11)).foregroundStyle(Theme.textFaint).padding(.top, 15)
                    }.padding(.vertical, 8)
                }.buttonStyle(.plain)
            }
        }
    }
}

private func statusLabel(_ status: String) -> String {
    switch status {
    case "working": return "Working"
    case "awaitingInput": return "Needs your input"
    case "errored": return "Needs attention"
    default: return "Ready"
    }
}

struct PairComputerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let model: CompanionModel
    @State private var code = ""
    @State private var error: String?
    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    Image(systemName: "link").font(.system(size: 36, weight: .light)).foregroundStyle(Theme.accent)
                    Text("Bring your computer along.").font(Theme.sans(29, weight: .semibold)).tracking(-0.8)
                    Text("Paste the private connection code created by the Noches connection gateway on your Mac or Linux host.")
                        .font(Theme.sans(16)).foregroundStyle(Theme.textMuted)
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
                    DisclosureGroup("Set up your host") {
                        Text("Run Noches on your computer, then use noches-connect pair to create a phone key and noches-connect serve to enable the connection. Use your host's Tailscale IP address and keep Tailscale connected on both devices. The phone cannot wake or start a stopped host service.")
                            .font(Theme.sans(14)).foregroundStyle(Theme.textMuted).padding(.top, 10)
                    }.font(Theme.sans(14))
                }.padding(24).frame(maxWidth: 640)
            }.background(Theme.bg).foregroundStyle(Theme.text)
                .navigationTitle("Pair a computer").navigationBarTitleDisplayMode(.inline)
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        }
    }
}

private struct NewHostSessionSheet: View {
    @Environment(\.dismiss) private var dismiss
    let model: CompanionModel
    let opened: (HostChat) -> Void
    @State private var space = ""
    @State private var harness = ""
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
                Section {
                    Button(busy ? "Creating…" : "Create session") {
                        busy = true
                        Task {
                            do {
                                let chat = try await model.create(spaceID: space.isEmpty ? nil : space, harness: harness)
                                dismiss(); opened(chat)
                            } catch { self.error = error.localizedDescription }
                            busy = false
                        }
                    }.disabled(busy || harness.isEmpty || !model.online)
                    if let error { Text(error).foregroundStyle(Theme.danger) }
                } footer: { Text("Uses the agent's default model. New sessions allow workspace writes and ask for approval when required.") }
            }.scrollContentBackground(.hidden).background(Theme.bg)
                .navigationTitle("New session").navigationBarTitleDisplayMode(.inline)
                .onAppear { harness = model.harnesses.first?.id ?? "" }
                .onChange(of: model.harnesses.map(\.id)) { _, ids in if harness.isEmpty { harness = ids.first ?? "" } }
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        }
    }
}

struct CompanionSessionView: View {
    let model: CompanionModel
    let chat: HostChat
    @State private var messages: [HostMessage] = []
    @State private var draft = ""
    @State private var error: String?
    @State private var busy = false
    @State private var loaded = false
    @State private var queue: [JSONValue] = []
    @State private var followTail = true
    @FocusState private var composerFocused: Bool
    private var currentChat: HostChat { model.chats.first { $0.id == chat.id } ?? chat }

    var body: some View {
        ScrollViewReader { scroll in
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 24) {
                Label("\(model.selected?.name ?? "Computer") · \(currentChat.cwd ?? "~")", systemImage: "desktopcomputer")
                    .font(Theme.mono(11)).foregroundStyle(Theme.textMuted)
                if !loaded { ProgressView("Loading session…") }
                if loaded && messages.isEmpty {
                    Text("What shall we work on?").font(Theme.sans(28, weight: .medium)).foregroundStyle(Theme.text).padding(.vertical, 40)
                }
                ForEach(messages) { message in
                    VStack(alignment: .leading, spacing: 12) {
                        Text(message.role == "user" ? "You" : message.role == "assistant" ? "Noches" : "System")
                            .font(Theme.mono(11)).foregroundStyle(Theme.textFaint)
                        ForEach(message.parts) { part in partView(part) }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(message.role == "user" ? 18 : 0)
                    .background(message.role == "user" ? Theme.surface : .clear, in: RoundedRectangle(cornerRadius: 20))
                }
                if !queue.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Queued on your computer").font(Theme.sans(13, weight: .medium))
                        ForEach(Array(queue.enumerated()), id: \.offset) { _, row in
                            Text(row.objectValue?["text"]?.stringValue ?? "Queued message").font(Theme.sans(14)).foregroundStyle(Theme.textMuted)
                        }
                    }.padding(16).modifier(CompanionPanel())
                }
                Color.clear.frame(height: 1).id("companion-tail")
            }.padding(24).frame(maxWidth: 760).frame(maxWidth: .infinity)
        }
        .defaultScrollAnchor(.bottom)
        .scrollDismissesKeyboard(.interactively)
        .onScrollGeometryChange(for: Bool.self) { geometry in
            geometry.visibleRect.maxY >= geometry.contentSize.height - 100
        } action: { _, nearBottom in followTail = nearBottom }
        .onChange(of: messages) { _, _ in
            if followTail { scroll.scrollTo("companion-tail", anchor: .bottom) }
        }
        .onChange(of: busy) { _, active in
            if active { followTail = true; scroll.scrollTo("companion-tail", anchor: .bottom) }
        }
        .toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button("Done") { composerFocused = false }
            }
        }
        .background(Theme.bg).foregroundStyle(Theme.text)
        .navigationTitle(currentChat.displayTitle).navigationBarTitleDisplayMode(.inline)
        .safeAreaInset(edge: .bottom) { composer }
        .task(id: "\(model.generation)-\(model.online)") { await watchTranscript() }
        .task(id: "queue-\(model.generation)-\(model.online)") {
            guard model.online else { return }
            do {
                for try await value in try await model.connection.watch("WatchQueue", ["chatId": chat.id]) {
                    queue = value.objectValue?["items"]?.arrayValue ?? []
                }
            } catch { if !Task.isCancelled { self.error = error.localizedDescription } }
        }
        }
    }

    @ViewBuilder private func partView(_ part: HostPart) -> some View {
        switch part.kind {
        case "text":
            let blocks = MarkdownParser.parse(part.text ?? "")
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                MarkdownBlockView(block: block.block, cacheKey: part.id).textSelection(.enabled)
            }
        case "tool":
            Label(part.call?.objectValue?["kind"]?.stringValue ?? "Tool", systemImage: part.isError == true ? "exclamationmark.circle" : "terminal")
                .font(Theme.mono(12)).foregroundStyle(part.isError == true ? Theme.danger : Theme.textMuted)
        case "error": Text(part.message ?? "The host reported an error.").foregroundStyle(Theme.danger)
        case "input" where part.resolved != true:
            QuestionPanel(requestId: part.requestId ?? part.id, questions: part.questions ?? []) { id, answers in
                perform {
                    try await model.respond(requestID: id, answers: answers, chat: currentChat)
                }
            }.disabled(busy || !model.online)
        case "image": Label("View generated image on your computer", systemImage: "photo").font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
        default: EmptyView()
        }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let error { Text(error).font(Theme.sans(12)).foregroundStyle(Theme.danger).textSelection(.enabled) }
            if !model.online { Text("Reconnecting to your computer…").font(Theme.sans(12)).foregroundStyle(Theme.warning) }
            HStack(alignment: .bottom, spacing: 12) {
                TextField("Message your agent…", text: $draft, axis: .vertical).lineLimit(1...6)
                    .font(Theme.sans(16)).focused($composerFocused).accessibilityIdentifier("companion-composer")
                Button {
                    let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
                    perform {
                        try await model.send(text, chat: currentChat)
                        if draft.trimmingCharacters(in: .whitespacesAndNewlines) == text { draft = "" }
                    }
                } label: { Image(systemName: "arrow.up").font(.system(size: 17, weight: .semibold)).frame(width: 44, height: 44)
                    .background(Theme.text, in: Circle()).foregroundStyle(Theme.bg) }
                    .accessibilityLabel("Send message")
                    .disabled(busy || !loaded || !model.online || draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            HStack {
                Text(statusLabel(model.status(chat.id))).font(Theme.mono(11)).foregroundStyle(Theme.textMuted)
                Spacer()
                if ["working", "awaitingInput"].contains(model.status(chat.id)) {
                    Button("Stop session", systemImage: "stop.fill") { perform { try await model.stop(currentChat) } }
                        .font(Theme.sans(12)).disabled(busy || !model.online)
                }
            }
        }.padding(16).modifier(CompanionPanel()).padding(.horizontal, 12).padding(.bottom, 8)
    }

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
