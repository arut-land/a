import AKit
import SwiftUI

struct ChatView: View {
    @Bindable var store: SessionStore
    @State private var input = ""

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
                if store.running {
                    Button("Stop", systemImage: "stop.circle") { Task { await store.interrupt() } }
                        .keyboardShortcut(".", modifiers: .command)
                }
                Button("Send", action: send)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(input.trimmingCharacters(in: .whitespaces).isEmpty || store.link == .closed)
            }
            .padding(10)
        }
        .navigationTitle(store.sessionId.map { String($0.suffix(8)) } ?? "New session")
        .navigationSubtitle(store.workspace)
        .toolbar {
            LinkBadge(link: store.link)
        }
    }

    private func send() {
        let text = input.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }
        input = ""
        Task { await store.send(text) }
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
            case .draining: ("reconnecting", .orange)
            case .closed: ("closed", .red)
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

struct ItemView: View {
    let item: ViewItem
    let store: SessionStore

    var body: some View {
        switch item {
        case .user(_, let text):
            HStack {
                Spacer(minLength: 80)
                Text(text)
                    .textSelection(.enabled)
                    .padding(.horizontal, 12).padding(.vertical, 8)
                    .background(.tint.opacity(0.15), in: .rect(cornerRadius: 12))
            }
        case .assistant(_, let text):
            Text(text).textSelection(.enabled)
        case .reply(let call, let model, let text, let status, let usage):
            ReplyView(
                call: call, model: model, text: text, status: status, usage: usage,
                streaming: live(call, status))
        case .tool(let call, _, _, let title, let approval, let outcome, _, let asked):
            ToolView(
                call: call, title: title, approval: approval, outcome: outcome, asked: asked,
                store: store)
        case .command(_, let name, let args, let outcome):
            VStack(alignment: .leading) {
                Note(icon: "slash.circle", text: "/\(name) \(args)")
                if let outcome { OutcomeView(outcome: outcome) }
            }
        case .hook(_, let hook, _, let line):
            Note(icon: "hook", text: "hook \(hook): \(line)")
        case .child(_, let child, let input, _, let fired):
            VStack(alignment: .leading) {
                Note(icon: "arrow.triangle.branch", text: "child \(child.suffix(8)): \(input)")
                if let fired { OutcomeView(outcome: fired) }
            }
        case .environment(_, let cwd):
            Note(icon: "folder", text: "environment: \(cwd)")
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

struct ReplyView: View {
    let call: UInt64
    let model: String
    let text: String
    let status: ReplyStatus
    let usage: TokenUsage?
    let streaming: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            switch status {
            case .running:
                HStack(alignment: .firstTextBaseline) {
                    Text(streaming ?? "").textSelection(.enabled)
                    ProgressView().controlSize(.small)
                }
            case .done:
                Text(text).textSelection(.enabled)
            case .failed(let error):
                Label("model failed: \(error)", systemImage: "xmark.octagon").foregroundStyle(.red)
            case .interrupted:
                Label("model call interrupted", systemImage: "pause.circle").foregroundStyle(.orange)
            }
            if let usage {
                Text("\(model) · \(usage.input) in, \(usage.output) out")
                    .font(.caption2).foregroundStyle(.tertiary)
            }
        }
    }
}

struct ToolView: View {
    let call: UInt64
    let title: String
    let approval: ApprovalState?
    let outcome: ToolOutcome?
    let asked: Bool
    let store: SessionStore

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Image(systemName: "terminal")
                Text(title).font(.body.monospaced())
                Spacer()
                switch approval {
                case .approved?: Text("approved").font(.caption).foregroundStyle(.green)
                case .denied?: Text("denied").font(.caption).foregroundStyle(.red)
                case .pending?: Text("waiting for approval").font(.caption).foregroundStyle(.orange)
                case nil: EmptyView()
                }
            }
            if approval == .pending, asked {
                HStack {
                    Button("Approve") { Task { await store.approve(call: call, approved: true) } }
                        .buttonStyle(.borderedProminent)
                    Button("Deny", role: .destructive) {
                        Task { await store.approve(call: call, approved: false) }
                    }
                }
            }
            if let outcome {
                OutcomeView(outcome: outcome)
            } else if approval != .pending {
                ProgressView().controlSize(.small)
            }
        }
        .padding(10)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 8))
    }
}

struct OutcomeView: View {
    let outcome: ToolOutcome

    var body: some View {
        switch outcome {
        case .done(_, let shown?):
            VStack(alignment: .leading, spacing: 2) {
                if !shown.text.isEmpty {
                    Text(shown.text.trimmingCharacters(in: .newlines))
                        .font(.callout.monospaced()).textSelection(.enabled)
                }
                if !shown.note.isEmpty {
                    Text(shown.note).font(.caption2).foregroundStyle(.secondary)
                }
            }
        case .done(let json, nil):
            Text(json).font(.callout.monospaced()).textSelection(.enabled)
        case .failed(let error):
            Label("failed: \(error)", systemImage: "xmark.octagon").foregroundStyle(.red)
        case .interrupted:
            Label("interrupted: the daemon stopped mid-call", systemImage: "pause.circle")
                .foregroundStyle(.orange)
        }
    }
}
