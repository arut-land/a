import AKit
import SwiftUI

struct ChatView: View {
    @Bindable var store: SessionStore
    /// What a typed line asks of the app, rather than of the session.
    var perform: (AppAction) -> Void = { _ in }
    @State private var input = ""
    @State private var showsHelp = false

    var body: some View {
        VStack(spacing: 0) {
            if let recap = store.recap {
                RecapBar(recap: recap)
            }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(store.items) { item in
                            ItemView(item: item, store: store)
                        }
                        Color.clear.frame(height: 1).id("end")
                    }
                    .padding()
                }
                .background(FollowEnd(store: store, proxy: proxy))
            }
            if let problem = store.problem {
                Label(problem, systemImage: "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal).padding(.vertical, 4)
            }
            Divider()
            HStack {
                TextField("Message, or $ command", text: $input)
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
        }
        .navigationTitle(store.title ?? store.sessionId.map { String($0.suffix(8)) } ?? "New session")
        .navigationSubtitle(store.workspace)
        .toolbar {
            LinkBadge(link: store.link)
            if case .notResponding = store.link {
                Button("Restart Daemon", systemImage: "arrow.clockwise") { Task { await store.restart() } }
            }
        }
        .popover(isPresented: $showsHelp) {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(store.commands, id: \.usage) { command in
                    HStack {
                        Text(command.usage).font(.body.monospaced())
                        Spacer(minLength: 16)
                        Text(command.help).foregroundStyle(.secondary)
                    }
                }
            }
            .padding()
        }
    }

    private func send() {
        let text = input.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }
        input = ""
        Task {
            switch await store.submit(text) {
            case .help?: showsHelp = true
            case let action?: perform(action)
            case nil: break
            }
        }
    }
}

/// Scrolls to the newest output as updates arrive. A view of its own, so a
/// streamed token re-evaluates it rather than the whole transcript.
struct FollowEnd: View {
    let store: SessionStore
    let proxy: ScrollViewProxy

    var body: some View {
        Color.clear.onChange(of: store.rev) { proxy.scrollTo("end", anchor: .bottom) }
    }
}

struct LinkBadge: View {
    let link: LinkState

    var body: some View {
        let (label, color): (String, Color) =
            switch link {
            case .connecting: ("connecting", .secondary)
            case .replaying: ("replaying", .orange)
            case .live: ("live", .green)
            case .draining, .reconnecting: ("reconnecting", .orange)
            case .notResponding: ("not responding", .red)
            case .detached: ("detached", .secondary)
            case .lost: ("closed", .red)
            }
        Label(label, systemImage: "circle.fill")
            .labelStyle(.titleAndIcon)
            .font(.caption)
            .foregroundStyle(color)
    }
}

struct RecapBar: View {
    let recap: RecapInfo

    var body: some View {
        Label(recap.line, systemImage: "clock.arrow.circlepath")
            .font(.caption)
            .lineLimit(2)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal).padding(.vertical, 6)
            .background(.quaternary)
    }
}

/// One transcript item, laid out by what it is and worded by how Rust
/// presents it (`shown`): the app decides neither status nor colour.
struct ItemView: View {
    let item: ViewItem
    let store: SessionStore

    var body: some View {
        let shown = item.shown()
        switch item {
        case .user:
            HStack {
                Spacer(minLength: 80)
                Text(shown.body?.text ?? "")
                    .textSelection(.enabled)
                    .padding(.horizontal, 12).padding(.vertical, 8)
                    .background(.tint.opacity(0.15), in: .rect(cornerRadius: 12))
            }
        case .assistant:
            BodyView(body: shown.body)
        // A side request's reply is its extension's, not the conversation's:
        // the title is the window's.
        case .reply(_, _, _, _, .some, _):
            EmptyView()
        case .reply(let call, _, _, let status, _, _):
            ReplyView(shown: shown, streaming: live(call, status))
        case .tool:
            ToolView(shown: shown, store: store)
        case .command where !shown.approval.isEmpty:
            ToolView(shown: shown, store: store)
        case .command:
            VStack(alignment: .leading) {
                HStack {
                    Note(icon: "slash.circle", text: shown.title)
                    StatusText(shown: shown)
                }
                BodyView(body: shown.body)
            }
        case .hook:
            Note(icon: "hook", text: shown.title)
        case .child:
            HStack {
                Note(icon: "arrow.triangle.branch", text: "\(shown.title): \(shown.body?.text ?? "")")
                StatusText(shown: shown)
            }
        case .environment:
            Note(icon: "folder", text: shown.title)
        }
    }
}

extension ItemView {
    /// The streamed text of a reply still running. Only such a reply reads
    /// it, so a token re-evaluates that reply alone.
    private func live(_ call: UInt64, _ status: ReplyStatus) -> String? {
        guard case .running = status, let streaming = store.streaming, streaming.call == call
        else { return nil }
        return streaming.text
    }
}

struct Note: View {
    let icon: String
    let text: String

    var body: some View {
        Label(text, systemImage: icon).font(.caption).foregroundStyle(.secondary)
    }
}

extension StatusGlyph {
    var color: Color {
        switch self {
        case .plain, .busy: .secondary
        case .ok: .green
        case .bad, .denied: .red
        case .warn, .ask: .orange
        }
    }
}

/// An item's status, in its class's colour; nothing when it has none.
struct StatusText: View {
    let shown: Presentation

    var body: some View {
        if !shown.status.isEmpty {
            Text(shown.status).font(.caption).foregroundStyle(shown.glyph.color)
        }
    }
}

/// What goes under an item's header: a reply's markdown as attributed
/// text, anything else preformatted.
struct BodyView: View {
    let body_: ShownBody?

    init(body: ShownBody?) { body_ = body }

    var body: some View {
        if let shown = body_ {
            switch shown.kind {
            case .markdown:
                Text(markdown(shown.text)).textSelection(.enabled)
            case .text, .diff, .json:
                Text(shown.text.trimmingCharacters(in: .newlines))
                    .font(.callout.monospaced()).textSelection(.enabled)
            }
        }
    }

    private func markdown(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(
            interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: text, options: options)) ?? AttributedString(text)
    }
}

struct ReplyView: View {
    let shown: Presentation
    let streaming: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if case .busy = shown.glyph {
                HStack(alignment: .firstTextBaseline) {
                    Text(streaming ?? "").textSelection(.enabled)
                    ProgressView().controlSize(.small)
                }
            } else if shown.status.isEmpty {
                BodyView(body: shown.body)
            } else {
                Label(shown.status, systemImage: "xmark.octagon").foregroundStyle(shown.glyph.color)
            }
            if let usage = shown.usage {
                Text("\(shown.title) · \(usage.input) in, \(usage.output) out")
                    .font(.caption2).foregroundStyle(.tertiary)
            }
        }
    }
}

struct ToolView: View {
    let shown: Presentation
    let store: SessionStore

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Image(systemName: "terminal")
                Text(shown.title).font(.body.monospaced())
                Spacer()
                StatusText(shown: shown)
                if case .busy = shown.glyph {
                    ProgressView().controlSize(.small)
                }
            }
            ForEach(Array(shown.approval.enumerated()), id: \.offset) { _, part in
                switch part {
                case .call(let text):
                    Text(text).font(.callout.monospaced()).textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                case .reason(let text), .unverifiedSummary(let text):
                    Text(text).font(.callout).textSelection(.enabled)
                case .controls(let call):
                    HStack {
                        Button("Approve") { Task { await store.approve(call: call, approved: true) } }
                            .buttonStyle(.borderedProminent)
                        Button("Deny", role: .destructive) {
                            Task { await store.approve(call: call, approved: false) }
                        }
                    }
                }
            }
            BodyView(body: shown.body)
        }
        .padding(10)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 8))
    }
}
