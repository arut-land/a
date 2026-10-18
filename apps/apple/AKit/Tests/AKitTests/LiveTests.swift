import Foundation
import Testing

@testable import AKit

/// A session against a real daemon, through the FFI. `mise run apple:test`
/// runs it with a throwaway `A_HOME` and the echo agent asking before
/// `bash`; without `A_TEST_HELPER` (the `a` to stage) it is skipped,
/// and it never runs against the default home.
nonisolated let live = ProcessInfo.processInfo.environment["A_TEST_HELPER"].flatMap { helper in
    ProcessInfo.processInfo.environment["A_HOME"] != nil ? helper : nil
}

/// Waits up to 10 s for `condition`.
func eventually(_ what: String, _ condition: () -> Bool) async throws {
    for _ in 0..<500 {
        if condition() { return }
        try await Task.sleep(for: .milliseconds(20))
    }
    Issue.record("timed out waiting until \(what)")
    throw CancellationError()
}

@Test(.enabled(if: live != nil))
func aLiveSessionChatsRunsAnApprovedToolAndIsListed() async throws {
    let client = try Client(helper: live!)
    let workspace = FileManager.default.temporaryDirectory
        .appending(path: "akit-live-\(ProcessInfo.processInfo.processIdentifier)")
    try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)

    let store = SessionStore(session: Session(client: client))
    await store.open(.new(workspace: workspace.path))
    try await eventually("live") { store.link == .live }
    let id = try #require(store.sessionId)

    await store.send("hi")
    try await eventually("the echo") {
        store.items.contains { if case .assistant(_, "hi") = $0 { true } else { false } }
    }

    await store.send("$ echo hello")
    try await eventually("the ask") { !store.pendingAsks.isEmpty }
    guard case .tool(let call, _, _, _, _, _, _, _) = store.pendingAsks[0] else { return }
    await store.approve(call: call, approved: true)
    try await eventually("the outcome") {
        store.items.contains {
            if case .tool(call, _, _, _, .approved?, .done(let output, let shown)?, let card, _) = $0 {
                output.contains(#""output":"hello\n""#) && card?.note == "exit 0"
                    && shown?.note == "exit 0"
            } else { false }
        }
    }

    let list = SessionListStore(client: client)
    await list.refresh()
    #expect(list.sessions.contains { $0.id == id && $0.clients >= 1 })

    // A second client of the same session folds to the same state.
    let other = SessionStore(session: Session(client: client))
    await other.open(.id(id: id))
    try await eventually("the replay and recap") {
        other.link == .live && other.seq == store.seq && other.recap != nil
    }
    #expect(other.items == store.items.map(unasked))
    // The recap reads as the Rust clients print it.
    #expect(other.recap?.line.hasPrefix("recap · you: $ echo hello · ran bash") == true)

    await store.detach()
    await other.detach()
    #expect(store.problem == nil && other.problem == nil)
}

/// `item` as a client that was not asked sees it.
func unasked(_ item: ViewItem) -> ViewItem {
    guard case .tool(let call, let tool, let input, let title, let approval, let outcome, let card, _) = item
    else { return item }
    return .tool(
        call: call, tool: tool, inputJson: input, title: title, approval: approval, outcome: outcome,
        card: card, asked: false)
}
