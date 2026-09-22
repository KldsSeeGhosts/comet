import SwiftUI

extension HostMessage {
    var renderedEntry: MessageEntry {
        MessageEntry(id: id, role: MessageRole(rawValue: role) ?? .system,
            parts: parts.compactMap { part in
                switch part.kind {
                case "text": return .text(id: part.id, text: part.text ?? "")
                case "tool":
                    let call = part.call?.objectValue ?? [:]
                    var fields: [String: AnyHashable] = [:]
                    for (key, value) in call where key != "kind" {
                        if let string = value.stringValue { fields[key] = string }
                        else if let data = try? JSONEncoder().encode(value) { fields[key] = String(decoding: data, as: UTF8.self) }
                    }
                    return .tool(id: part.id, call: RenderToolCall(tag: call["kind"]?.stringValue ?? "unknown", fields: fields),
                        isError: part.isError ?? false, resolved: part.isError != nil)
                case "input": return .input(id: part.id, requestId: part.requestId ?? part.id, questions: part.questions ?? [], resolved: part.resolved ?? false)
                case "error": return .error(id: part.id, message: part.message ?? "The host reported an error.")
                case "image": return .text(id: part.id, text: "*View this generated image on your computer.*")
                default: return nil
                }
            }, createdAt: 0, deviceId: "", status: status.flatMap(MessageStatus.init(rawValue:)), continuationOf: nil)
    }
}

/// Reuses Noches' tested native transcript, incremental Markdown parser,
/// tool disclosures, text folding, and gesture-owned scroll state.
struct CompanionTranscript: View {
    let messages: [HostMessage]
    let scroll: ScrollState
    let online: Bool
    let busy: Bool
    let submittedID: String?
    let respond: (String, [UserInputAnswer]) -> Void
    @State private var rows: [TranscriptRow] = []
    @State private var cache = TranscriptBuilderCache()
    @State private var revision: UInt64 = 0
    @State private var veils = VeilStore()
    @State private var expanded = Set<String>()
    @State private var openTools = Set<String>()
    @State private var expansionHeights: [String: CGFloat] = [:]
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    var body: some View {
        NativeTranscriptTable(rows: rows, scroll: scroll, runwayID: submittedID,
            expansionHeight: submittedID.flatMap { expansionHeights[$0] } ?? 0,
            bottomSpacing: 18, reduceMotion: reduceMotion,
            configurationID: expanded.hashValue ^ openTools.hashValue ^ dynamicTypeSize.hashValue ^ online.hashValue ^ busy.hashValue) { row in
                AnyView(content(row).padding(.top, row.topGap).padding(.horizontal, 20)
                    .frame(maxWidth: TranscriptView.maxContentWidth).frame(maxWidth: .infinity)
                    .environment(\.dynamicTypeSize, dynamicTypeSize))
            }
            // The shared table renders beyond its viewport for keyboard motion.
            // Keep that overscan out of the companion navigation/status bars.
            .clipped()
            .background(Theme.bg)
            .onChange(of: messages, initial: true) { _, value in
                revision &+= 1
                rows = cache.rows(revision: revision, entries: value.map(\.renderedEntry), pendingSends: [])
            }
            .overlay(alignment: .bottomTrailing) {
                if scroll.showJump {
                    Button { scroll.arm(); scroll.jumpToLatest?(!reduceMotion) } label: {
                        Image(systemName: "arrow.down").font(.system(size: 16, weight: .medium)).frame(width: 44, height: 44)
                    }.nochesGlass(.regular.interactive(), in: Circle())
                        .accessibilityLabel("Jump to latest").padding(12)
                }
            }
    }

    @ViewBuilder private func content(_ row: TranscriptRow) -> some View {
        switch row.kind {
        case .user(let text):
            UserBubble(text: text, pending: false, deviceId: "", expanded: expanded.contains(row.entryId), onToggle: {
                scroll.pinned = false
                if !expanded.insert(row.entryId).inserted { expanded.remove(row.entryId) }
                scroll.refreshLayout?()
            }, onExpansionHeightChanged: { height in expansionHeights[row.entryId] = height; scroll.refreshLayout?() })
        case .markdown(let block, let streaming):
            MarkdownRowView(row: row, block: block, streaming: streaming, veils: veils)
        case .toolGroup(let tools, _):
            ToolGroupView(tools: tools, open: openTools.contains(row.id), userToggled: true, toggle: {
                if !openTools.insert(row.id).inserted { openTools.remove(row.id) }
                scroll.refreshLayout?()
            }, onDetailChanged: { scroll.refreshLayout?() })
        case .inputChip(let header, let resolved):
            if !resolved, let part = messages.first(where: { $0.id == row.entryId })?.parts.first(where: { "\(row.entryId)#\($0.id)" == row.id }) {
                QuestionPanel(requestId: part.requestId ?? part.id, questions: part.questions ?? [], respond: respond)
                    .disabled(!online || busy)
            } else { InputChipView(header: header, resolved: resolved) }
        case .errorChip(let message): ErrorChipView(message: message)
        case .generatedImage: EmptyView()
        }
    }
}
