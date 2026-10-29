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
    defer { try? FileManager.default.removeItem(at: workspace) }

    let session = Session(client: client)
    let store = SessionStore(session: session)
    store.open(.new(workspace: workspace.path))
    try await eventually("live") { store.link == .live }
    let id = try #require(store.sessionId)

    await store.send("hi")
    try await eventually("the echo") {
        store.items.contains { $0.kind == .assistant && $0.presentation.body?.text == "hi" }
    }

    await store.send("$ echo hello")
    try await eventually("the ask and its requested tool row") {
        guard let call = store.turn.asking else { return false }
        return store.items.contains { $0.kind == .tool && $0.key == call && $0.presentation.approval.contains(.controls(call: call)) }
    }
    let call = store.turn.asking!
    await store.approve(call: call, approved: true)
    try await eventually("the outcome") {
        store.items.contains {
            $0.kind == .tool && $0.key == call && $0.presentation.body?.text == "hello" && $0.presentation.status == "exit 0"
        }
    }

    let details = try #require(session.details(key: call))
    #expect(details.contains(#""output":"hello\n""#))

    let list = SessionListStore(client: client)
    let following = Task { await list.follow() }
    try await eventually("the session listed") {
        list.sessions.contains { $0.id == id && $0.clients >= 1 }
    }

    let other = SessionStore(session: Session(client: client))
    other.open(.id(id: id))
    try await eventually("the replay and recap") {
        other.link == .live && other.seq == store.seq && other.recap != nil && other.history?.pending == false
    }
    #expect(other.items == store.items)
    #expect(other.recap?.line.hasPrefix("recap · you: $ echo hello · ran bash") == true)

    #expect(!store.turn.running && !store.turn.canInterrupt)
    #expect(try await store.submit("/help") == .help)
    #expect(store.commands.contains { $0.usage == "/interrupt" })
    await store.send("$ sleep 30")
    try await eventually("the second ask") { store.turn.asking != nil }
    await store.approve(call: store.turn.asking!, approved: true)
    try await eventually("the call to run") { store.turn.running && store.turn.asking == nil }
    #expect(store.turn.canInterrupt)
    #expect(try await store.submit("/interrupt") == nil)
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

@Test(.enabled(if: live != nil))
func detached_session_reopens_with_a_new_handle() async throws {
    let client = try Client(helper: live!)
    let cache = SessionStoreCache()
    let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-detach-\(UUID())")
    try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: workspace) }
    weak var handle: Session?
    let store: SessionStore = {
        let session = Session(client: client)
        handle = session
        return SessionStore(session: session)
    }()
    store.open(.new(workspace: workspace.path))
    try await eventually("attached") { store.link == .live }
    let id = try #require(store.sessionId)
    cache.insert(store, id: id)
    cache.selection = id

    await store.detach()
    #expect(store.attachment == .detached)
    #expect(handle == nil)
    #expect(cache.stores[id] == nil && cache.selection == nil)
    cache.selection = id
    let reopened = cache.open(id, client: client)
    #expect(reopened !== store)
    try await eventually("reopened") { reopened.link == .live }
    #expect(reopened.attachment == .attached(id: id))
    await reopened.send("reopened")
    try await eventually("new handle receives replies") {
        reopened.items.contains { $0.kind == .assistant && $0.presentation.body?.text == "reopened" }
    }
    await reopened.detach()
}

@Test(.enabled(if: live != nil))
func fork_rekeys_the_cached_store() async throws {
    let client = try Client(helper: live!)
    let cache = SessionStoreCache()
    let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-fork-\(UUID())")
    try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: workspace) }
    let store = SessionStore(session: Session(client: client))
    store.open(.new(workspace: workspace.path))
    try await eventually("attached") { store.link == .live }
    let original = try #require(store.sessionId)
    cache.insert(store, id: original)
    cache.selection = original
    await store.send("before fork")
    try await eventually("fork rest point") { !store.turn.running && store.items.contains {
        $0.kind == .assistant && $0.presentation.body?.text == "before fork"
    } }

    _ = try await store.submit("/fork")
    try await eventually("fork attached") { store.link == .live && store.sessionId != original }
    let fork = try #require(store.sessionId)
    #expect(cache.stores[original] == nil)
    #expect(cache.stores[fork] === store)
    #expect(cache.selection == fork)
    #expect(cache.stores.count == 1)
    let reopened = cache.open(original, client: client)
    #expect(reopened !== store)
    try await eventually("original reattached") { reopened.link == .live }
    #expect(reopened.sessionId == original && store.sessionId == fork)
    await reopened.detach()
    await store.detach()
}


@Test
func apple_startup_does_not_block_the_main_actor() async throws {
    let started = DispatchSemaphore(value: 0)
    let release = DispatchSemaphore(value: 0)
    let startup = Task {
        try await stageClient {
            started.signal()
            release.wait()
            return 42
        }
    }
    try await stageClient { started.wait() }
    // The main actor must run this task while staging is still suspended.
    let heartbeat = Task { @MainActor in
        await Task.yield()
        release.signal()
    }
    await heartbeat.value
    #expect(try await startup.value == 42)
}


@Test(.enabled(if: live != nil))
func daemon_diagnosis_is_fetched_asynchronously() async throws {
    let client = try Client(helper: live!)
    _ = try await client.settings()
    #expect(try await client.daemonPid() != nil)
}

@Test(.enabled(if: live != nil))
func evicted_sessions_release_connections() async throws {
    let client = try Client(helper: live!)
    let cache = SessionStoreCache(capacity: 0)
    let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-cache-\(UUID())")
    try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: workspace) }
    weak var handle: Session?
    let store: SessionStore = {
        let session = Session(client: client)
        handle = session
        return SessionStore(session: session)
    }()
    store.open(.new(workspace: workspace.path))
    try await eventually("attached") { store.link == .live }
    let id = try #require(store.sessionId)
    cache.selection = id
    cache.insert(store, id: id)
    #expect(store.cacheState == .active)
    let list = SessionListStore(client: client)
    let following = Task { await list.follow() }
    defer { following.cancel() }
    try await eventually("listed") { list.sessions.contains { $0.id == id && $0.clients == 1 } }
    cache.selection = nil
    #expect(cache.stores.isEmpty)
    #expect(store.cacheState == .detached)
    try await eventually("handle released") { handle == nil }
    try await eventually("connection released") { list.sessions.contains { $0.id == id && $0.clients == 0 } }

    let bounded = SessionStoreCache(capacity: 1, bytes: 0)
    let reopened = bounded.open(id, client: client)
    try await eventually("byte budget evicts inactive views") { bounded.stores.isEmpty }
    #expect(reopened.cacheState == .detached)
}

@Test(.enabled(if: live != nil))
func aRejectedSubmissionThrowsWithoutConsumingTheDraft() async throws {
    let client = try Client(helper: live!)
    let store = SessionStore(session: Session(client: client))
    store.open(.new(workspace: FileManager.default.temporaryDirectory.path))
    try await eventually("live for admission") { store.link == .live }
    let draft = "/interrupt"
    await #expect(throws: ClientError.self) { try await store.submit(draft) }
    #expect(store.problem != nil)
    #expect(!store.turn.running)
    #expect(try await store.submit("/help") == .help)
    await store.detach()
}

@Test(.enabled(if: live != nil))
func demanded_history_replaces_the_native_window_without_losing_the_session() async throws {
    let client = try Client(helper: live!)
    let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-history-\(UUID())")
    try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: workspace) }
    let store = SessionStore(session: Session(client: client))
    store.open(.new(workspace: workspace.path))
    try await eventually("the first range") { store.link == .live && store.history?.pending == false }
    for turn in 0..<4 {
        let marker = "end of turn \(turn)"
        await store.send(String(repeating: "history line\n", count: 12_000) + marker)
        try await eventually("the committed tail") {
            !store.turn.running && store.history?.pending == false && store.items.contains {
                $0.kind == .assistant && $0.presentation.body?.text.hasSuffix(marker) == true
            }
        }
    }
    let session = store.sessionId
    let tail = try #require(store.history)
    let cut = tail.cut
    var windows = 0
    while let history = store.history, history.start > 0 || history.textStart > 0 {
        let revision = store.rev
        store.loadEarlier()
        try await eventually("the preceding demanded range") { store.rev > revision && store.history?.pending == false }
        #expect(store.sessionId == session && store.history?.cut == cut)
        #expect(store.items.reduce(0) { $0 + ($1.presentation.body?.text.utf8.count ?? 0) } < 70_000)
        windows += 1
    }
    #expect(windows > 4)
    while let history = store.history, history.end < history.total || history.textEnd < history.textTotal {
        let revision = store.rev
        store.loadLater()
        try await eventually("the succeeding demanded range") { store.rev > revision && store.history?.pending == false }
        #expect(store.sessionId == session && store.history?.cut == cut)
    }
    store.loadLater()
    try await eventually("following the latest range") {
        store.followsHistory && store.history?.pending == false && store.history?.start == tail.start && store.history?.textStart == tail.textStart
    }
    let revision = store.rev
    store.loadEarlier()
    try await eventually("leaving the live range") { store.rev > revision && store.history?.pending == false }
    #expect(!store.followsHistory)
    store.followLatest()
    try await eventually("returning directly to the latest reply") {
        store.followsHistory && store.history?.pending == false && store.history?.start == tail.start && store.history?.textStart == tail.textStart && store.items.contains {
            $0.kind == .assistant && $0.presentation.body?.text.hasSuffix("end of turn 3") == true
        }
    }
    #expect(store.history?.cut == cut)
    await store.detach()
}
