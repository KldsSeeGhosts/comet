import SwiftUI

struct HostDirectory: Decodable {
    let directory: String
    let entries: [HostFileEntry]
    let nextCursor: String?
    let truncated: Bool
}
struct HostFileEntry: Decodable, Identifiable, Hashable {
    let path: String
    let name: String
    let kind: String
    var id: String { path }
}
struct HostFileText: Decodable {
    let path: String
    let text: String?
    let encoding: String
    let truncated: Bool
}
struct HostDiff: Decodable {
    let patch: String
    let additions: Int
    let deletions: Int
    let truncated: Bool
    let files: [File]
    struct File: Decodable, Identifiable {
        let path: String
        let additions: Int
        let deletions: Int
        var id: String { path }
    }
}

enum CompanionInspector: String, Identifiable {
    case files, changes
    var id: String { rawValue }
}

struct CompanionWorkspaceSheet: View {
    @Environment(\.dismiss) private var dismiss
    let model: CompanionModel
    let chat: HostChat
    let tab: CompanionInspector
    var body: some View {
        NavigationStack {
            Group {
                if tab == .files { CompanionDirectoryView(model: model, chat: chat, directory: "", close: { dismiss() }) }
                else { CompanionChangesView(model: model, chat: chat) }
            }
            .toolbar { if tab == .changes { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } } }
        }.tint(Theme.text).presentationDragIndicator(.visible)
    }
}

struct CompanionDirectoryView: View {
    let model: CompanionModel
    let chat: HostChat
    let directory: String
    var close: () -> Void = {}
    @State private var entries: [HostFileEntry] = []
    @State private var cursor: String?
    @State private var partial = false
    @State private var loaded = false
    @State private var busy = false
    @State private var error: String?
    var body: some View {
        List {
            if let error { CompanionReadError(message: error) { Task { await load(more: false) } } }
            if !loaded && error == nil { ProgressView("Loading files…") }
            if loaded && entries.isEmpty { Text("This folder is empty.").foregroundStyle(Theme.textMuted) }
            ForEach(entries) { file in
                NavigationLink(value: file) {
                    Label(file.name, systemImage: file.kind == "directory" ? "folder" : file.kind == "symlink" ? "link" : "doc.text")
                        .font(Theme.sans(14)).padding(.vertical, 6)
                }.listRowBackground(Theme.bg)
            }
            if cursor != nil {
                Button(busy ? "Loading…" : "Load more files") { Task { await load(more: true) } }.disabled(busy)
            } else if partial { Text("The host returned a partial directory listing.").font(Theme.sans(12)).foregroundStyle(Theme.textMuted) }
        }
        .listStyle(.plain).scrollContentBackground(.hidden).background(Theme.bg).foregroundStyle(Theme.text)
        .navigationTitle(directory.isEmpty ? "Files" : (directory as NSString).lastPathComponent)
        .navigationBarTitleDisplayMode(.inline)
        .navigationDestination(for: HostFileEntry.self) { file in
            if file.kind == "directory" { CompanionDirectoryView(model: model, chat: chat, directory: file.path, close: close) }
            else { CompanionFileView(model: model, chat: chat, path: file.path, close: close) }
        }
        .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done", action: close) } }
        .task(id: model.generation) { await load(more: false) }
        .refreshable { await load(more: false) }
    }
    private func load(more: Bool) async {
        guard !busy else { return }
        busy = true; error = nil
        defer { busy = false }
        do {
            var params: [String: Any] = ["chatId": chat.id, "directory": directory]
            if more { params["cursor"] = cursor }
            let page = try CompanionModel.decode(HostDirectory.self, await model.read("ListWorkspaceDirectory", chat: chat, params: params))
            guard !Task.isCancelled else { return }
            let merged = (more ? entries : []) + page.entries
            var seen = Set<String>()
            entries = merged.filter { seen.insert($0.id).inserted }
            cursor = page.nextCursor; partial = page.truncated; loaded = true
        } catch { if !Task.isCancelled { self.error = error.localizedDescription } }
    }
}

struct CompanionFileView: View {
    let model: CompanionModel
    let chat: HostChat
    let path: String
    var close: () -> Void = {}
    @State private var file: HostFileText?
    @State private var error: String?
    var body: some View {
        Group {
            if let file {
                if let text = file.text {
                    CompanionCodeView(text: text, diff: false, partial: file.truncated)
                } else {
                    ContentUnavailableView("Preview unavailable", systemImage: "doc", description: Text("This file uses \(file.encoding) encoding. Open it on your computer."))
                }
            } else if let error { CompanionReadError(message: error) { Task { await load() } }.padding(24) }
            else { ProgressView("Reading file…") }
        }.frame(maxWidth: .infinity, maxHeight: .infinity).background(Theme.bg).foregroundStyle(Theme.text)
            .navigationTitle((path as NSString).lastPathComponent).navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done", action: close) } }
            .task(id: model.generation) { await load() }
    }
    private func load() async {
        error = nil; file = nil
        do {
            let result = try CompanionModel.decode(HostFileText.self, await model.read("ReadWorkspaceFile", chat: chat, params: ["chatId": chat.id, "path": path]))
            guard !Task.isCancelled else { return }; file = result
        } catch { if !Task.isCancelled { self.error = error.localizedDescription } }
    }
}

struct CompanionChangesView: View {
    let model: CompanionModel
    let chat: HostChat
    @State private var diff: HostDiff?
    @State private var error: String?
    var body: some View {
        Group {
            if let diff {
                if diff.files.isEmpty {
                    ContentUnavailableView("No uncommitted changes", systemImage: "checkmark", description: Text("No changes in this working tree."))
                } else {
                    VStack(spacing: 0) {
                        HStack(spacing: 14) {
                            Text("\(diff.files.count) changed \(diff.files.count == 1 ? "file" : "files")").foregroundStyle(Theme.textMuted)
                            Spacer()
                            Text("+\(diff.additions)").foregroundStyle(Theme.statusCompleted)
                            Text("−\(diff.deletions)").foregroundStyle(Theme.danger)
                        }.font(Theme.mono(12)).padding(20)
                        CompanionCodeView(text: diff.patch, diff: true, partial: diff.truncated)
                    }
                }
            } else if let error { CompanionReadError(message: error) { Task { await load() } }.padding(24) }
            else { ProgressView("Loading changes…") }
        }.frame(maxWidth: .infinity, maxHeight: .infinity).background(Theme.bg).foregroundStyle(Theme.text)
            .navigationTitle("Changes").navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .topBarLeading) { Button("Refresh", systemImage: "arrow.clockwise") { Task { await load() } } } }
            .task(id: model.generation) { await load() }
    }
    private func load() async {
        error = nil; diff = nil
        do {
            let result = try CompanionModel.decode(HostDiff.self, await model.read("GetCheckoutDiff", chat: chat,
                params: ["cwd": chat.cwd ?? "~", "chatId": chat.id, "mode": "working"]))
            guard !Task.isCancelled else { return }; diff = result
        } catch { if !Task.isCancelled { self.error = error.localizedDescription } }
    }
}

struct CompanionCodeView: View {
    let text: String
    let diff: Bool
    let partial: Bool
    private let limit = 200_000
    var body: some View {
        ScrollView([.horizontal, .vertical]) {
            LazyVStack(alignment: .leading, spacing: 0) {
                if partial || text.count > limit {
                    Text("Partial preview. Open the full file on your computer.")
                        .font(Theme.sans(12)).foregroundStyle(Theme.warning).padding(.vertical, 12)
                }
                ForEach(Array(String(text.prefix(limit)).components(separatedBy: "\n").enumerated()), id: \.offset) { index, line in
                    HStack(alignment: .top, spacing: 14) {
                        if !diff { Text(String(index + 1)).foregroundStyle(Theme.textFaint).frame(width: 34, alignment: .trailing) }
                        Text(line.isEmpty ? " " : line).foregroundStyle(color(line)).textSelection(.enabled)
                    }.font(Theme.mono(12)).frame(minHeight: 21)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(diff && line.hasPrefix("+") ? Theme.statusCompleted.opacity(0.07) : diff && line.hasPrefix("-") ? Theme.danger.opacity(0.07) : .clear)
                }
            }.padding(16)
        }.background(Theme.bg)
    }
    private func color(_ line: String) -> Color {
        guard diff else { return Theme.text }
        if line.hasPrefix("+") { return Theme.statusCompleted }
        if line.hasPrefix("-") { return Theme.danger }
        if line.hasPrefix("@@") { return Theme.accent }
        return Theme.textMuted
    }
}
struct CompanionReadError: View {
    let message: String
    let retry: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(message).font(Theme.sans(14)).foregroundStyle(Theme.textMuted)
            Button("Try again", action: retry).font(Theme.sans(14, weight: .medium)).frame(minHeight: 44)
        }
    }
}
