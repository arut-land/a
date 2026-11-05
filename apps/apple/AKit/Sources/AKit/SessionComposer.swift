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
                        Task {
                            if let text = try? await store.editQueued(queued.seq) { input = text; managing = false; composing = true }
                        }
                    }
                }.font(.caption).padding(.horizontal, 10)
            }
            Divider()
            HStack {
                TextField(composerPlaceholder, text: $input)
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
