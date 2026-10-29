import AKit
import SwiftUI

struct ChatView: View {
    @Bindable var store: SessionStore
    /// What a typed line asks of the app, rather than of the session.
    var perform: (AppAction) -> Void = { _ in }
    @State private var input = ""
    @State private var showsHelp = false
    @State private var userScrolling = false

    var body: some View {
        VStack(spacing: 0) {
            if let recap = store.recap {
                RecapBar(recap: recap)
            }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        if let history = store.history, (history.start > 0 || history.textStart > 0) {
                            Button("Earlier messages") { store.loadEarlier() }
                                .disabled(history.pending)
                        }
                        ForEach(store.items) { item in
                            ItemView(item: item, store: store).id(item.key)
                        }
                        if !store.followsHistory {
                            HStack {
                                Button("Later messages") { store.loadLater() }
                                    .disabled(store.history?.pending == true)
                                Button("Latest messages") { store.followLatest() }
                            }
                        }
                        Color.clear.frame(height: 1).id("end")
                    }
                    .padding()
                }
                .scrollBounceBehavior(.always, axes: .vertical)
                .onScrollPhaseChange { _, phase, context in
                    userScrolling = phase.isScrolling && phase != .animating
                    if userScrolling { page(HistoryEdge.of(context.geometry)) }
                }
                .onScrollGeometryChange(for: HistoryEdge.self) { HistoryEdge.of($0) } action: { _, edge in
                    if userScrolling { page(edge) }
                }
                .onChange(of: store.history.map { PageAnchor(start: $0.start, text: $0.textStart) }) {
                    guard !store.followsHistory else { return }
                    switch store.historyDirection {
                    case .older:
                        if let item = store.items.last { proxy.scrollTo(item.key, anchor: .bottom) }
                    case .newer:
                        if let item = store.items.first { proxy.scrollTo(item.key, anchor: .top) }
                    }
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

    private func page(_ edge: HistoryEdge) {
        switch edge {
        case .older: store.loadEarlier()
        case .newer: if !store.followsHistory { store.loadLater() }
        case .middle: break
        }
    }

    private func send() {
        let text = input.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }
        let draft = input
        Task {
            do {
                let action = try await store.submit(text)
                if input == draft { input = "" }
                switch action {
                case .help?: showsHelp = true
                case let action?: perform(action)
                case nil: break
                }
            } catch {}
        }
    }
}

private struct PageAnchor: Equatable {
    let start: UInt64
    let text: UInt64
}

private enum HistoryEdge: Equatable {
    case older, newer, middle

    static func of(_ geometry: ScrollGeometry) -> Self {
        let y = geometry.contentOffset.y + geometry.contentInsets.top
        if y <= 0 { return .older }
        if y + geometry.containerSize.height >= geometry.contentSize.height { return .newer }
        return .middle
    }
}

/// Streamed tokens update this view without rebuilding the transcript.
struct FollowEnd: View {
    let store: SessionStore
    let proxy: ScrollViewProxy

    var body: some View {
        Color.clear.onChange(of: store.rev) { if store.followsHistory { proxy.scrollTo("end", anchor: .bottom) } }
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
    let item: DisplayItem
    let store: SessionStore

    var body: some View {
        let shown = item.presentation
        switch item.kind {
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
        case .side:
            EmptyView()
        case .reply:
            ReplyView(shown: shown, streaming: live(item.key))
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
    private func live(_ call: UInt64) -> String? {
        guard let streaming = store.streaming, streaming.call == call
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
                Text(markdown(shown)).textSelection(.enabled)
            case .text, .diff, .json:
                Text(shown.text.trimmingCharacters(in: .newlines))
                    .font(.callout.monospaced()).textSelection(.enabled)
            }
        }
    }

    private func markdown(_ shown: ShownBody) -> AttributedString {
        if let runs = shown.runs {
            return runs.reduce(into: AttributedString()) { result, run in
                var text = AttributedString(run.text)
                switch run.style {
                case .text: break
                case .bold: text.font = .body.bold()
                case .code: text.font = .body.monospaced()
                }
                result += text
            }
        }
        let text = shown.text
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
            if let question = shown.question { QuestionForm(question: question, store: store).id(question.id) }
            BodyView(body: shown.body)
        }
        .padding(10)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 8))
    }
}


private struct QuestionForm: View {
    let question: UserQuestion
    let store: SessionStore
    @State private var values: [String: String] = [:]
    @State private var included: Set<String> = []

    private func value(_ key: String) -> Binding<String> {
        Binding(get: { values[key] ?? "" }, set: { values[key] = $0 })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Requested by \(question.source)").font(.headline)
            ForEach(Array(question.prompts.enumerated()), id: \.offset) { _, prompt in
                Text(prompt.message).textSelection(.enabled)
                if question.prompts.count > 1 {
                    Picker("Response for this prompt", selection: value(prompt.key)) {
                        Text("Accept").tag("accept")
                        Text("Decline").tag("decline")
                        Text("Cancel").tag("cancel")
                    }
                }
                if let url = prompt.url {
                    Text(url).font(.callout.monospaced()).textSelection(.enabled)
                    if let destination = URL(string: url) {
                        Link("Open URL in browser", destination: destination)
                    }
                }
                ForEach(prompt.fields, id: \.key) { field in
                    VStack(alignment: .leading) {
                        if !field.required {
                            Toggle("Include \(field.title)", isOn: Binding(get: { included.contains(field.key) }, set: { if $0 { included.insert(field.key) } else { included.remove(field.key) } }))
                        }
                        Text(field.title + (field.required ? " (required)" : ""))
                        if let description = field.description { Text(description).font(.caption) }
                        switch field.kind {
                        case .boolean:
                            Picker(field.title, selection: value(field.key)) {
                                Text("Choose…").tag("")
                                Text("Yes").tag("true")
                                Text("No").tag("false")
                            }
                        case .choice:
                            Picker(field.title, selection: Binding<String?>(get: { values[field.key] }, set: { values[field.key] = $0 })) {
                                Text("Choose…").tag(nil as String?)
                                ForEach(field.options, id: \.value) { Text($0.title).tag(Optional($0.value)) }
                            }
                        case .multipleChoice:
                            ForEach(field.options, id: \.value) { option in
                                Toggle(option.title, isOn: Binding(get: {
                                    let selected = (try? JSONDecoder().decode([String].self, from: Data((values[field.key] ?? "[]").utf8))) ?? []
                                    return selected.contains(option.value)
                                }, set: { checked in
                                    var selected = (try? JSONDecoder().decode([String].self, from: Data((values[field.key] ?? "[]").utf8))) ?? []
                                    selected.removeAll { $0 == option.value }
                                    if checked { selected.append(option.value) }
                                    if let data = try? JSONEncoder().encode(selected) { values[field.key] = String(decoding: data, as: UTF8.self) }
                                }))
                            }
                        case .number, .text:
                            TextField(field.title, text: value(field.key))
                                .textFieldStyle(.roundedBorder)
                        }
                    }
                }
            }
            HStack {
                Button("Submit / Continue") { send(.accept) }.buttonStyle(.borderedProminent)
                Button("Decline") { send(.decline) }
                Button("Cancel") { send(.cancel) }
            }
        }
        .onAppear {
            for prompt in question.prompts { values[prompt.key] = "accept" }
            for field in question.prompts.flatMap(\.fields) {
                if let initial = field.initial { values[field.key] = initial; included.insert(field.key) }
                if field.kind == .multipleChoice && field.initial == nil { values[field.key] = "[]" }
                if field.required { included.insert(field.key) }
            }
        }
    }

    private func send(_ action: QuestionAction) {
        let draft = question.prompts.map { QuestionInput(key: $0.key, value: values[$0.key] ?? "accept", included: true) } + question.prompts.flatMap(\.fields).map { field in
            QuestionInput(key: field.key, value: values[field.key] ?? "", included: (field.required || included.contains(field.key)) && (field.kind != .choice || values[field.key] != nil))
        }
        Task { await store.answer(question: question, action: action, values: draft) }
    }
}
