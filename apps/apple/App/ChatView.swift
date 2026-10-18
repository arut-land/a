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
                .onChange(of: store.rev) { proxy.scrollTo("end", anchor: .bottom) }
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
        var parts: [String] = []
        if let prompt = recap.prompt { parts.append("you: \(prompt)") }
        if !recap.tools.isEmpty { parts.append("ran \(recap.tools.joined(separator: ", "))") }
        if recap.failed > 0 { parts.append("\(recap.failed) failed") }
        if recap.running > 0 { parts.append("\(recap.running) running") }
        if let reply = recap.reply { parts.append("a: \(reply)") }
        if recap.asks > 0 { parts.append("\(recap.asks) waiting for approval") }
        return Label(parts.joined(separator: " · "), systemImage: "clock.arrow.circlepath")
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
                streaming: store.streaming?.call == call ? store.streaming?.text : nil)
        case .tool(let call, let tool, let input, let approval, let outcome, let card, let asked):
            ToolView(
                call: call, tool: tool, input: input, approval: approval, outcome: outcome,
                card: card, asked: asked, store: store)
        case .command(_, let name, let args, let outcome):
            VStack(alignment: .leading) {
                Note(icon: "slash.circle", text: "/\(name) \(args)")
                if let outcome { OutcomeView(outcome: outcome, card: nil) }
            }
        case .hook(_, let hook, let decision):
            Note(icon: "hook", text: "hook \(hook): \(Self.decision(decision))")
        case .child(_, let child, let input, _, let fired):
            VStack(alignment: .leading) {
                Note(icon: "arrow.triangle.branch", text: "child \(child.suffix(8)): \(input)")
                if let fired { OutcomeView(outcome: fired, card: nil) }
            }
        case .environment(_, let cwd):
            Note(icon: "folder", text: "environment: \(cwd)")
        }
    }

    /// A hook decision, `{"kind": {fields}}` on the wire, as `kind field=value…`.
    static func decision(_ json: String) -> String {
        guard let object = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any],
            let (kind, value) = object.first
        else { return json }
        let fields = (value as? [String: Any] ?? [:]).sorted { $0.key < $1.key }
        return ([kind] + fields.map { "\($0.key)=\($0.value)" }).joined(separator: " ")
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
    let tool: String
    let input: String
    let approval: ApprovalState?
    let outcome: ToolOutcome?
    let card: ToolCard?
    let asked: Bool
    let store: SessionStore

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Image(systemName: "terminal")
                Text(card?.title ?? Self.title(tool: tool, input: input)).font(.body.monospaced())
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
                OutcomeView(outcome: outcome, card: card)
            } else if approval != .pending {
                ProgressView().controlSize(.small)
            }
        }
        .padding(10)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 8))
    }

    /// A command as `$ command`, anything else as the tool and its input.
    static func title(tool: String, input: String) -> String {
        let object = try? JSONSerialization.jsonObject(with: Data(input.utf8)) as? [String: Any]
        if let command = object?["command"] as? String { return "$ \(command)" }
        return "\(tool) \(input)"
    }
}

struct OutcomeView: View {
    let outcome: ToolOutcome
    /// How the tool presents it, when it does.
    let card: ToolCard?

    var body: some View {
        if case .done = outcome, let card, !card.note.isEmpty {
            VStack(alignment: .leading, spacing: 2) {
                if !card.body.isEmpty {
                    Text(card.body.trimmingCharacters(in: .newlines))
                        .font(.callout.monospaced()).textSelection(.enabled)
                }
                Text(card.note).font(.caption2).foregroundStyle(.secondary)
            }
        } else {
            RawOutcome(outcome: outcome)
        }
    }
}

/// An outcome without a card: a command's streams as older logs hold them,
/// anything else as JSON.
struct RawOutcome: View {
    let outcome: ToolOutcome

    var body: some View {
        switch outcome {
        case .done(let json):
            let output = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
            if let stdout = output?["stdout"] as? String {
                let stderr = output?["stderr"] as? String ?? ""
                VStack(alignment: .leading, spacing: 2) {
                    Text((stdout + stderr).trimmingCharacters(in: .newlines))
                        .font(.callout.monospaced()).textSelection(.enabled)
                    Text("exit \(output?["exit"].map { "\($0)" } ?? "?")")
                        .font(.caption2).foregroundStyle(.secondary)
                }
            } else {
                Text(json).font(.callout.monospaced()).textSelection(.enabled)
            }
        case .failed(let error):
            Label("failed: \(error)", systemImage: "xmark.octagon").foregroundStyle(.red)
        case .interrupted:
            Label("interrupted: the daemon stopped mid-call", systemImage: "pause.circle")
                .foregroundStyle(.orange)
        }
    }
}
