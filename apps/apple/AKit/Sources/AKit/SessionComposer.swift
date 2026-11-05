import Observation
import SwiftUI

/// The same composer, queue and picker footer for every session in a family.
public struct SessionComposer: View {
    private let store: SessionStore
    private let conversations: ConversationStore?
    @Binding private var input: String
    @Binding private var managing: Bool
    @FocusState private var composing: Bool
    private let send: () -> Void
    private let open: (String) -> Void

    public init(store: SessionStore, conversations: ConversationStore?, input: Binding<String>, managing: Binding<Bool>, send: @escaping () -> Void, open: @escaping (String) -> Void) {
        self.store = store
        self.conversations = conversations
        self._input = input
        self._managing = managing
        self.send = send
        self.open = open
    }

    public var body: some View {
        VStack(spacing: 0) {
            ForEach(store.queuedInputs, id: \.seq) { queued in
                HStack {
                    Text("Queued: \(queued.text)").lineLimit(2)
                    Spacer()
                    Button("Edit") {
                        let ticket = store.composerDrafts.begin(session: store.sessionId, draft: input)
                        Task {
                            if let text = try? await store.editQueued(queued.seq),
                               let replacement = store.composerDrafts.complete(ticket, text: text, session: store.sessionId, draft: input) {
                                input = replacement; managing = false; composing = true
                            }
                        }
                    }
                }.font(.caption).padding(.horizontal, 10)
            }
            ForEach(Array(store.composerDrafts.saved.enumerated()), id: \.offset) { index, draft in
                if draft.session == store.sessionId {
                    Button("Restore draft: " + draft.text) {
                        input = store.composerDrafts.restore(index, draft: input, session: store.sessionId)
                        managing = false; composing = true
                    }.font(.caption)
                }
            }
            Divider()
            HStack {
                TextField(composerPlaceholder, text: Binding(get: { input }, set: { store.composerDrafts.changed(); input = $0 }))
                    .focused($composing)
                    .simultaneousGesture(TapGesture().onEnded { managing = false; composing = true })
                    .onChange(of: composing) { _, active in if active { managing = false } }
                    .onKeyPress(.downArrow) {
                        guard input.isEmpty, conversations?.rows.isEmpty == false else { return .ignored }
                        composing = false; managing = true
                        return .handled
                    }
                    .textFieldStyle(.plain)
                    .onSubmit(send)
                if store.turn.canInterrupt {
                    Button("Stop", systemImage: "stop.circle") { Task { await store.interrupt() } }
                        .keyboardShortcut(".", modifiers: .command)
                }
                Button("Send", action: send)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(input.trimmingCharacters(in: .whitespaces).isEmpty || !store.turn.canSend)
            }
            .padding(10)
            if let conversations {
                Divider()
                ConversationSwitcher(store: conversations, current: store.sessionId, managing: $managing, open: open)
                    .onChange(of: managing) { _, active in if !active { composing = true } }
            }
        }
        .onDisappear { store.composerDrafts.changed() }
        .onChange(of: input) { _, _ in store.composerDrafts.changed() }
        .onAppear { if let id = store.sessionId { conversations?.view(id) } }
        .onChange(of: store.sessionId) { _, id in if let id { conversations?.view(id) } }
    }

    private var composerPlaceholder: String {
        if let row = conversations?.rows.first(where: { $0.id == store.sessionId }), row.parent != nil {
            return "Message @\(row.title)…"
        }
        return "Message, or $ command"
    }

}

@MainActor @Observable
final class ComposerDrafts {
    struct Ticket { let revision: UInt64; let session: String?; let draft: String }
    struct Saved { let session: String?; let text: String }
    private var revision: UInt64 = 0
    private(set) var saved: [Saved] = []

    func changed() { revision += 1 }
    func begin(session: String?, draft: String) -> Ticket { Ticket(revision: revision, session: session, draft: draft) }
    func complete(_ ticket: Ticket, text: String, session: String?, draft: String) -> String? {
        guard ticket.revision == revision, ticket.session == session, ticket.draft == draft else {
            retain(text, session: ticket.session)
            return nil
        }
        retain(draft, session: session)
        changed()
        return text
    }
    func restore(_ index: Int, draft: String, session: String?) -> String {
        let recalled = saved.remove(at: index)
        retain(draft, session: session)
        changed()
        return recalled.text
    }
    private func retain(_ text: String, session: String?) {
        if !text.isEmpty && !saved.contains(where: { $0.session == session && $0.text == text }) { saved.append(Saved(session: session, text: text)) }
    }
}
