import AKit
import SwiftUI

struct ContentView: View {
    @Bindable var model: AppModel

    var body: some View {
        NavigationSplitView {
            SessionList(model: model)
                .navigationSplitViewColumnWidth(min: 220, ideal: 260)
        } detail: {
            if let draft = model.draft {
                ChatView(store: draft, perform: model.perform)
                    .onChange(of: draft.sessionId) { model.adoptDraft() }
            } else if let id = model.selection, let store = model.stores[id] {
                ChatView(store: store, perform: model.perform).id(id)
            } else if let problem = model.problem {
                ContentUnavailableView(
                    "Cannot reach the daemon", systemImage: "exclamationmark.triangle",
                    description: Text(problem))
            } else {
                ContentUnavailableView(
                    "No session selected", systemImage: "bubble.left.and.text.bubble.right",
                    description: Text("Pick a session, or start one with ⌘N."))
            }
        }
        .alert(
            model.notice ?? "", isPresented: Binding(get: { model.notice != nil }, set: { if !$0 { model.notice = nil } })
        ) {}
    }
}

struct SessionList: View {
    @Bindable var model: AppModel

    var body: some View {
        List(selection: $model.selection) {
            if let list = model.list {
                ForEach(list.sessions) { session in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(session.short).font(.body.monospaced())
                        Text(URL(filePath: session.workspace).lastPathComponent)
                            .font(.caption).foregroundStyle(.secondary)
                        Text("\(session.activity) · \(session.residency) · \(session.entries) entries · \(session.clients) attached")
                            .font(.caption2)
                            .foregroundStyle(session.requiresAction ? AnyShapeStyle(.orange) : AnyShapeStyle(.tertiary))
                    }
                    .tag(session.id)
                }
                if let problem = list.problem {
                    Text(problem).font(.caption).foregroundStyle(.red)
                }
            }
        }
        .onChange(of: model.selection) { _, id in
            if let id { model.open(id) }
        }
        .toolbar {
            Button("New Session", systemImage: "plus") { model.chooseWorkspace() }
        }
        .task {
            await model.list?.follow { model.openRequested() }
        }
    }
}
