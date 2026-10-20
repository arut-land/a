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
    store.open(.new(workspace: workspace.path))
    try await eventually("live") { store.link == .live }
    let id = try #require(store.sessionId)

    await store.send("hi")
    try await eventually("the echo") {
        store.items.contains { if case .assistant(_, "hi", _) = $0 { true } else { false } }
    }

    await store.send("$ echo hello")
    try await eventually("the ask") { store.turn.asking != nil }
    let call = store.turn.asking!
    #expect(store.items.contains { if case .tool(call, _, _, _, let shown) = $0 { shown.approve == call } else { false } })
    await store.approve(call: call, approved: true)
    try await eventually("the outcome") {
        store.items.contains {
            if case .tool(call, _, _, .done(let output)?, let shown) = $0 {
                output.contains(#""output":"hello\n""#) && shown.status == "exit 0"
            } else { false }
        }
    }

    // The daemon pushes the list; nothing polls it.
    let list = SessionListStore(client: client)
    let following = Task { await list.follow() }
    try await eventually("the session listed") {
        list.sessions.contains { $0.id == id && $0.clients >= 1 }
    }

    // A second client of the same session folds to the same state.
    let other = SessionStore(session: Session(client: client))
    other.open(.id(id: id))
    try await eventually("the replay and recap") {
        other.link == .live && other.seq == store.seq && other.recap != nil
    }
    #expect(other.items == store.items)
    // The recap reads as the Rust clients print it.
    #expect(other.recap?.line.hasPrefix("recap · you: $ echo hello · ran bash") == true)

    // A running call stops on an interrupt.
    #expect(!store.turn.running && !store.turn.canInterrupt)
    // Typed lines go through the commands every client shares.
    #expect(await store.submit("/help") == .help)
    #expect(store.commands.contains { $0.usage == "/interrupt" })
    await store.send("$ sleep 30")
    try await eventually("the second ask") { store.turn.asking != nil }
    await store.approve(call: store.turn.asking!, approved: true)
    try await eventually("the call to run") { store.turn.running && store.turn.asking == nil }
    #expect(store.turn.canInterrupt)
    #expect(await store.submit("/interrupt") == nil)
    try await eventually("the call to stop") { !store.turn.running }

    await store.detach()
    await other.detach()
    #expect(store.problem == nil && other.problem == nil)
    // A detach is never a lost daemon.
    #expect(store.link == .detached && other.link == .detached)
    try await eventually("the list to show them gone") {
        list.sessions.contains { $0.id == id && $0.clients == 0 }
    }
    following.cancel()
}
