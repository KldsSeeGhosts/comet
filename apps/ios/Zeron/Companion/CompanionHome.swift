import SwiftUI

/// A native session list, with the desktop's real assets and color roles.
struct CompanionDashboard: View {
    let model: CompanionModel
    let newSession: (String) -> Void
    let pair: () -> Void
    let opened: (HostChat) -> Void
    @State private var project = ""
    @State private var query = ""
    @State private var scope: CompanionScope = .all
    @FocusState private var searchFocused: Bool

    private var visible: [HostChat] { model.visibleChats(project: project, query: query, scope: scope) }
    private var groups: [String] {
        var seen = Set<String>()
        return visible.compactMap { seen.insert($0.spaceId ?? "").inserted ? ($0.spaceId ?? "") : nil }
    }

    var body: some View {
        List {
            connectionRow
                .listRowInsets(EdgeInsets(top: 2, leading: 20, bottom: 12, trailing: 20))
                .listRowSeparator(.hidden).listRowBackground(Color.clear)
            if !model.online {
                Text(model.error ?? "Connect Tailscale and keep Noches running on your computer.")
                    .font(Theme.sans(13)).foregroundStyle(Theme.warning)
                    .listRowBackground(Color.clear).listRowSeparator(.hidden)
            }
            if visible.isEmpty {
                emptyState.listRowBackground(Color.clear).listRowSeparator(.hidden)
            }
            ForEach(groups, id: \.self) { id in
                Section {
                    ForEach(visible.filter { ($0.spaceId ?? "") == id }) { chat in
                        CompanionSessionRow(model: model, chat: chat) {
                            searchFocused = false
                            UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                            opened(chat)
                        }
                        .listRowInsets(EdgeInsets(top: 0, leading: 20, bottom: 0, trailing: 20))
                        .listRowSeparator(.hidden).listRowBackground(Color.clear)
                    }
                } header: {
                    HStack(spacing: 10) {
                        Text(model.localSpaces.first { $0.id == id }?.displayName ?? "No project")
                            .font(Theme.sans(12, weight: .medium)).foregroundStyle(Theme.textMuted)
                        Rectangle().fill(Theme.border).frame(height: 0.5)
                        Button { searchFocused = false; newSession(id) } label: {
                            Image(systemName: "plus").font(.system(size: 12)).frame(width: 34, height: 34)
                        }.disabled(!model.online).accessibilityLabel("New session in \(model.localSpaces.first { $0.id == id }?.displayName ?? "home folder")")
                    }.textCase(nil).listRowInsets(EdgeInsets(top: 0, leading: 20, bottom: 0, trailing: 16))
                }
            }
        }
        .listStyle(.plain).scrollContentBackground(.hidden)
        .scrollDismissesKeyboard(.interactively)
        .background(Theme.bg).foregroundStyle(Theme.text)
        .safeAreaInset(edge: .bottom, spacing: 0) { bottomBar }
        .onAppear { searchFocused = false }
        .onChange(of: model.selectedID) { _, _ in project = ""; query = ""; scope = .all }
    }

    private var connectionRow: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 6) {
                Menu {
                    ForEach(model.profiles) { host in
                        Button { model.select(host.id) } label: {
                            if host.id == model.selectedID { Label(host.name, systemImage: "checkmark") }
                            else { Text(host.name) }
                        }
                    }
                    Divider()
                    Button("Pair a computer", systemImage: "plus", action: pair)
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "desktopcomputer").font(.system(size: 11))
                        Text(model.selected?.name ?? "Computer").font(Theme.sans(12)).lineLimit(1)
                        Image(systemName: "chevron.down").font(.system(size: 8, weight: .medium))
                    }.foregroundStyle(Theme.textMuted).frame(minHeight: 36)
                }.accessibilityLabel("Switch computer")
                Spacer()
                Circle().fill(model.online ? Theme.statusCompleted : Theme.warning).frame(width: 4, height: 4)
                Text(model.connectionMessage).font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
            }
            HStack {
                Menu {
                    Button("All projects") { project = "" }
                    ForEach(model.localSpaces) { space in Button(space.displayName) { project = space.id } }
                } label: {
                    HStack(spacing: 7) {
                        Image(systemName: "folder").font(.system(size: 12))
                        Text(model.localSpaces.first { $0.id == project }?.displayName ?? "All projects")
                            .font(Theme.sans(13, weight: .medium))
                        Image(systemName: "chevron.down").font(.system(size: 8))
                    }.foregroundStyle(Theme.text)
                }
                Spacer()
                Text(scope == .all ? "\(visible.count) sessions" : "\(scope.rawValue) · \(visible.count)")
                    .font(Theme.mono(10)).foregroundStyle(Theme.textFaint)
            }.frame(minHeight: 28)
        }
    }

    private var bottomBar: some View {
        HStack(spacing: 8) {
            Menu {
                ForEach(CompanionScope.allCases, id: \.self) { item in
                    Button { scope = item } label: {
                        if item == scope { Label(item.rawValue, systemImage: "checkmark") } else { Text(item.rawValue) }
                    }.accessibilityIdentifier("scope-\(item.rawValue)")
                }
            } label: {
                Image(systemName: scope == .all ? "line.3.horizontal.decrease" : "line.3.horizontal.decrease.circle.fill")
                    .font(.system(size: 18, weight: .regular)).frame(width: 46, height: 46)
                    .nochesGlass(in: Circle())
            }.accessibilityLabel("Filter sessions")
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass").font(.system(size: 14)).foregroundStyle(Theme.textMuted)
                TextField("Search", text: $query).font(Theme.sans(15))
                    .autocorrectionDisabled().textInputAutocapitalization(.never)
                    .accessibilityIdentifier("session-search").focused($searchFocused)
                    .submitLabel(.search).onSubmit { searchFocused = false }
                if !query.isEmpty {
                    Button { query = "" } label: { Image(systemName: "xmark.circle.fill").font(.system(size: 15)).foregroundStyle(Theme.textMuted) }
                        .frame(width: 30, height: 44).accessibilityLabel("Clear search")
                }
            }.padding(.horizontal, 15).frame(height: 46).nochesGlass(in: Capsule())
            Button { searchFocused = false; newSession("") } label: {
                Image(systemName: "square.and.pencil").font(.system(size: 18)).frame(width: 46, height: 46)
                    .nochesGlass(in: Circle())
            }.disabled(!model.online).accessibilityLabel("New session").accessibilityIdentifier("new-session")
        }
        .padding(.horizontal, 16).padding(.top, 10).padding(.bottom, 8)
        .background {
            LinearGradient(colors: [Theme.bg.opacity(0), Theme.bg.opacity(0.94), Theme.bg], startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea(edges: .bottom)
        }
    }

    private var emptyState: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(!query.isEmpty ? "No matching sessions" : scope == .attention ? "Nothing needs your attention" : scope == .working ? "No agents working" : scope == .archived ? "No archived sessions" : "Start a session")
                .font(Theme.sans(17, weight: .medium))
            Text(!query.isEmpty ? "Search by title, branch, or project." : scope == .archived ? "Archived sessions can be restored here." : "Your sessions stay in sync with your computer.")
                .font(Theme.sans(14)).foregroundStyle(Theme.textMuted)
        }.padding(.vertical, 30)
    }
}

struct CompanionSessionRow: View {
    let model: CompanionModel
    let chat: HostChat
    let opened: () -> Void
    @State private var rename = false
    @State private var name = ""
    @State private var error: String?
    @State private var busy = false
    private var status: String { model.status(chat.id) }

    var body: some View {
        Button(action: opened) {
            HStack(alignment: .top, spacing: 12) {
                CompanionAvatar(status: status, seed: chat.id).padding(.top, 1)
                VStack(alignment: .leading, spacing: 6) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(chat.displayTitle).font(Theme.sans(15, weight: .medium))
                            .foregroundStyle(Theme.text).lineLimit(2).multilineTextAlignment(.leading)
                        Spacer(minLength: 0)
                        if ["working", "awaitingInput", "errored"].contains(status) {
                            Text(status == "awaitingInput" ? "Input" : statusLabel(status))
                                .font(Theme.sans(11)).foregroundStyle(companionStatusColor(status)).fixedSize()
                        } else { Text(relativeTime).font(Theme.mono(10)).foregroundStyle(Theme.textFaint).fixedSize() }
                    }
                    if let preview = chat.lastMessagePreview, !preview.isEmpty {
                        Text(preview).font(Theme.sans(12)).foregroundStyle(Theme.textMuted).lineLimit(1)
                    }
                    HStack(spacing: 6) {
                        Text(chat.branch ?? (chat.cwd as NSString?)?.lastPathComponent ?? "home")
                            .font(Theme.mono(10)).lineLimit(1)
                        Spacer(minLength: 4)
                        if let harness = chat.config?.harness {
                            BrandMarkShape(mark: .forHarness(harness))
                                .fill(BrandMark.tint(for: harness), style: FillStyle(eoFill: BrandMark.forHarness(harness).evenOddFill))
                                .frame(width: 12, height: 12)
                        }
                    }.foregroundStyle(Theme.textFaint)
                }
            }.padding(.vertical, 15).contentShape(Rectangle())
        }
        .buttonStyle(.plain).accessibilityIdentifier("open-session-\(chat.id)")
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.border.opacity(0.5)).frame(height: 0.5).padding(.leading, 48) }
        .contextMenu {
            Button("Rename session", systemImage: "pencil") { name = chat.displayTitle; rename = true }
            Button(chat.archived ? "Restore session" : "Archive session", systemImage: "archivebox") { archive() }
        }
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            Button(chat.archived ? "Restore" : "Archive", systemImage: chat.archived ? "tray.and.arrow.up" : "archivebox") { archive() }
                .tint(Theme.textMuted)
        }
        .disabled(busy)
        .alert("Rename session", isPresented: $rename) {
            TextField("Session name", text: $name)
            Button("Cancel", role: .cancel) {}
            Button("Save") { perform { try await model.rename(chat, title: name) } }
                .disabled(!model.online || name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .alert("Could not update session", isPresented: Binding(get: { error != nil }, set: { if !$0 { error = nil } })) {
            Button("OK", role: .cancel) { error = nil }
        } message: { Text(error ?? "") }
    }
    private var relativeTime: String {
        guard let date = ISO8601DateFormatter().date(from: chat.lastMessageAt ?? chat.createdAt) else { return "" }
        let seconds = max(0, Int(Date().timeIntervalSince(date)))
        if seconds < 60 { return "now" }
        if seconds < 3600 { return "\(seconds / 60)m" }
        if seconds < 86400 { return "\(seconds / 3600)h" }
        return "\(seconds / 86400)d"
    }
    private func archive() { perform { try await model.archive(chat, archived: !chat.archived) } }
    private func perform(_ action: @escaping () async throws -> Void) {
        busy = true
        Task { do { try await action() } catch { self.error = error.localizedDescription }; busy = false }
    }
}

struct CompanionAvatar: View {
    let status: String
    let seed: String
    private var asset: String {
        let avatars = ["bot-orbit", "bot-visor", "bot-dome", "bot-box", "bot-ears", "bot-halo", "bot-sprout", "bot-bolt", "bot-basic"]
        let hash = seed.utf8.reduce(UInt8(0)) { ($0 &* 31) &+ $1 }
        return avatars[Int(hash) % avatars.count]
    }
    var body: some View {
        Image(asset).resizable().scaledToFit().frame(width: 32, height: 34)
            .frame(width: 36, height: 36)
            .overlay(alignment: .bottomTrailing) {
                Circle().fill(companionStatusColor(status)).frame(width: 7, height: 7)
                    .overlay(Circle().stroke(Theme.bg, lineWidth: 2)).offset(x: 1, y: 1)
            }.accessibilityHidden(true)
    }
}

func companionStatusColor(_ status: String) -> Color {
    switch status {
    case "working": Theme.statusWorking
    case "awaitingInput": Theme.warning
    case "errored": Theme.danger
    case "completed": Theme.statusCompleted
    default: Theme.textFaint
    }
}
struct CompanionSessionMenu: View {
    let model: CompanionModel
    let chat: HostChat
    var archived: () -> Void = {}
    @State private var renaming = false
    @State private var title = ""
    @State private var error: String?
    @State private var busy = false
    var body: some View {
        Menu {
            Button("Rename session", systemImage: "pencil") { title = chat.displayTitle; renaming = true }
            Button(chat.archived ? "Restore session" : "Archive session", systemImage: chat.archived ? "tray.and.arrow.up" : "archivebox") {
                perform { try await model.archive(chat, archived: !chat.archived); archived() }
            }
        } label: {
            Image(systemName: "ellipsis").font(.system(size: 16)).foregroundStyle(Theme.textMuted).frame(width: 44, height: 44)
        }
        .accessibilityLabel("Session actions for \(chat.displayTitle)")
        .disabled(!model.online || busy)
        .alert("Rename session", isPresented: $renaming) {
            TextField("Session name", text: $title)
            Button("Cancel", role: .cancel) {}
            Button("Save") { perform { try await model.rename(chat, title: title) } }
                .disabled(title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .alert("Could not update session", isPresented: Binding(get: { error != nil }, set: { if !$0 { error = nil } })) {
            Button("OK", role: .cancel) { error = nil }
        } message: { Text(error ?? "") }
    }
    private func perform(_ action: @escaping () async throws -> Void) {
        busy = true
        Task {
            do { try await action() } catch { self.error = error.localizedDescription }
            busy = false
        }
    }
}
