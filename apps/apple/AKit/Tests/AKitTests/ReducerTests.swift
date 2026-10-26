import Foundation
import Testing
import Synchronization

@testable import AKit

/// The shared reducer vectors (`A_VECTORS`, as `mise run apple:test` names
/// them), run through the FFI: every client folds them to the same state.
struct Vector {
    let name: String
    let messages: [String]
    let updates: [(rev: UInt64, seq: UInt64)]
    let state: NSDictionary
    let hash: String

    static let all: [Vector] = {
        guard let path = ProcessInfo.processInfo.environment["A_VECTORS"] else {
            fatalError("A_VECTORS names the reducer vectors; run `mise run apple:test`")
        }
        let data = try! Data(contentsOf: URL(filePath: path))
        let vectors = try! JSONSerialization.jsonObject(with: data) as! [[String: Any]]
        return vectors.map { v in
            // Preserve the wire bytes: rebuilding JSON objects changes raw argument ordering.
            let messages = v["frames"] as! [String]
            let updates = (v["updates"] as! [[String: Any]]).map {
                (($0["rev"] as! NSNumber).uint64Value, ($0["seq"] as! NSNumber).uint64Value)
            }
            return Vector(
                name: v["name"] as! String, messages: messages, updates: updates,
                state: v["state"] as! NSDictionary, hash: v["hash"] as! String)
        }
    }()

    /// Folds the messages into a fresh reducer.
    func fold() throws -> (Reducer, [ChangeBatch]) {
        let reducer = Reducer()
        let updates = try messages.compactMap { try reducer.fold(message: $0) }
        return (reducer, updates)
    }
}

@Test func theVectorsFoldToTheSameStateAndHash() throws {
    #expect(!Vector.all.isEmpty)
    for vector in Vector.all {
        let (reducer, updates) = try vector.fold()
        #expect(updates.map(\.revision) == vector.updates.map(\.rev), "\(vector.name)")
        #expect(updates.map(\.seq) == vector.updates.map(\.seq), "\(vector.name)")
        let state = try JSONSerialization.jsonObject(with: Data(reducer.stateJson().utf8))
        #expect((state as! NSDictionary) == vector.state, "\(vector.name)")
        #expect(reducer.snapshot().stateHash == vector.hash, "\(vector.name)")
    }
}

/// What a store shows, compared with the Rust view it follows.
func expectInStep(_ store: SessionStore, _ reducer: Reducer, _ name: String) {
    let snapshot = reducer.snapshot()
    #expect(store.items == snapshot.items, "\(name)")
    #expect(store.streaming == snapshot.streaming, "\(name)")
    #expect(store.recap == snapshot.recap, "\(name)")
    #expect((store.rev, store.seq) == (snapshot.rev, snapshot.seq), "\(name)")
    #expect(store.sessionId == snapshot.session, "\(name)")
    #expect(store.workspace == snapshot.workspace, "\(name)")
    #expect(store.turn == snapshot.turn, "\(name)")
}

@Test func aStoreAppliesTheUpdatesToTheSameView() throws {
    for vector in Vector.all {
        let reducer = Reducer()
        let store = SessionStore(source: reducer)
        for message in vector.messages {
            if let update = try reducer.fold(message: message) {
                store.apply(update)
                store.apply(update)
                let snapshot = reducer.snapshot()
                #expect(store.items == snapshot.items, "\(vector.name)")
                #expect(store.streaming == snapshot.streaming, "\(vector.name)")
                #expect(store.turn == snapshot.turn, "\(vector.name)")
                #expect(store.rev == snapshot.rev && store.seq == snapshot.seq, "\(vector.name)")
            }
        }
        expectInStep(store, reducer, vector.name)
        #expect(store.resyncs == 0)
    }
}

@Test func aStoreCatchesUpOnDroppedUpdates() async throws {
    for vector in Vector.all {
        let (reducer, updates) = try vector.fold()
        let store = SessionStore(source: reducer)
        for (i, update) in updates.enumerated() where i % 3 != 1 {
            store.apply(update)
        }
        try await eventually("dropped updates recover") { store.rev == reducer.snapshot().rev }
        expectInStep(store, reducer, vector.name)
        #expect(updates.count < 3 || store.resyncs > 0, "\(vector.name)")
    }
}

@Test func aStoreFarBehindTakesASnapshot() async throws {
    let reducer = Reducer()
    let vector = Vector.all.first { $0.name == "a far behind renderer takes a snapshot" }!
    let store = SessionStore(source: reducer)
    var last: ChangeBatch?
    for message in vector.messages {
        last = try reducer.fold(message: message)
    }
    store.apply(last!)
    try await eventually("far behind recovers") { store.rev == reducer.snapshot().rev }
    expectInStep(store, reducer, "far behind")
    #expect(store.items.count == 1 && store.resyncs == 1)
}

/// An interrupt settles a call waiting for approval, so Stop shows for one.
@Test func aCallWaitingForApprovalCanBeStopped() throws {
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    let vector = Vector.all.first { $0.name == "approval reason precedes its optional description" }!
    for message in vector.messages.prefix(4) {
        if let update = try reducer.fold(message: message) { store.apply(update) }
    }
    #expect(store.items.count == 1)
    #expect(store.turn.running && store.turn.canInterrupt)
    #expect(store.turn == reducer.turn())
}

/// A side request titling the session titles the store; its reply is no
/// reply of the conversation's.
@Test func aTitlingReplyTitlesTheStore() throws {
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    let vector = Vector.all.first { $0.name == "a titling reply titles the store" }!
    for message in vector.messages {
        if let update = try reducer.fold(message: message) { store.apply(update) }
    }
    #expect(store.title == "Fix the parser")
    #expect(store.items.first?.kind == .side)

}

@Test func errorsCarryStableCodes() {
    #expect(throws: ClientError.self) { try Reducer().fold(message: "{") }
    do {
        _ = try Reducer().fold(message: "{")
    } catch let error as ClientError {
        #expect(error.code() == "protocol")
    } catch {
        Issue.record("unexpected \(error)")
    }
}

@Test func approvalReasonsShowBeforeDescriptionsAndSurviveReplay() throws {
    let vector = Vector.all.first { $0.name == "approval reason precedes its optional description" }!
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    var sawQuestion = false
    for message in vector.messages {
        if let update = try reducer.fold(message: message) { store.apply(update) }
        if let shown = store.items.first?.presentation, !shown.approval.isEmpty {
            if shown.approval.count == 3 { sawQuestion = true }
        }
    }
    #expect(sawQuestion)
    let parts = store.items.first!.presentation.approval
    #expect(parts.count == 4)
    #expect(parts[1] == .reason(text: "Why: the ask mode requires approval for this tool"))
    #expect(parts[2] == .unverifiedSummary(text: "Model summary (unverified): Lists files in the current directory."))
    let (replayed, _) = try vector.fold()
    #expect(replayed.snapshot().items.first!.presentation.approval == parts)
    expectInStep(store, reducer, vector.name)
}

@Test func everySummaryFollowsCompleteEffectiveInputBeforeControls() throws {
    let vectors = Vector.all.filter { $0.name.hasPrefix("approval reviews ") }
    #expect(vectors.count == 4)
    for vector in vectors {
        let (reducer, _) = try vector.fold()
        let parts = reducer.snapshot().items.first!.presentation.approval
        guard parts.count == 4,
            case .call(let input) = parts[0],
            case .reason = parts[1],
            case .unverifiedSummary(let summary) = parts[2],
            case .controls = parts[3] else {
            Issue.record("The app must render complete input, reason, unverified summary, controls in order")
            continue
        }
        #expect(summary.hasPrefix("Model summary (unverified):"))
        #expect(!input.contains("original harmless input"))
        let markers = vector.name.contains("multiline")
            ? ["printf harmless\nrm -rf /tmp/approval-test-sentinel", "60000", "every argument"]
            : ["/work/deep/config.ini", "-allow = false", "+allow = true", "-delete = no", "+delete = yes", "flag"]
        for marker in markers { #expect(input.contains(marker), "\(vector.name): \(marker)") }
    }
}

@Test func display_batches_apply_under_one_revision() throws {
    let vector = Vector.all.first { $0.name == "approval reviews the effective multiline call" }!
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    var sawBatch = false
    for message in vector.messages {
        if let update = try reducer.fold(message: message) {
            let previous = store.rev
            store.apply(update)
            #expect(store.rev == previous + 1)
            expectInStep(store, reducer, vector.name)
            if update.changes.count > 1 { sawBatch = true }
        }
    }
    #expect(sawBatch)
    #expect(store.resyncs == 0)
}

@Test func anInvalidDisplayBatchLeavesItemsAndRevisionUnchanged() throws {
    let vector = Vector.all.first { $0.name == "approval reviews the effective multiline call" }!
    let (reducer, _) = try vector.fold()
    let item = reducer.snapshot().items[0]
    let store = SessionStore(source: Reducer())
    let batch = ChangeBatch(
        revision: 1, retainedBytes: 0, seq: 99,
        changes: [
            .added(index: 0, item: item),
            .updated(index: 10, item: item),
            .error(message: "must not be applied"),
        ],
        turn: reducer.turn(), streaming: nil, title: "must not be applied")
    store.apply(batch)
    #expect(store.items.isEmpty)
    #expect(store.rev == 0)
    #expect(store.seq == 0)
    #expect(store.title == nil)
}

@Test func rejectedDisplayBatchesRespectResetsAndSnapshotsAndAllowRetry() throws {
    let vector = Vector.all.first { $0.name == "approval reviews the effective multiline call" }!
    let (reducer, _) = try vector.fold()
    let item = reducer.snapshot().items[0]
    let next = reducer.snapshot().items[1]
    let invalid: [[ViewChange]] = [
        [.added(index: 2, item: next)],
        [.snapshot(session: nil, workspace: "", items: []), .updated(index: 0, item: item)],
        [.attached(session: "discarded", workspace: "/discarded", reset: true), .updated(index: 0, item: item)],
        [.added(index: 1, item: next), .updated(index: 2, item: item)],
    ]
    for changes in invalid {
        let store = SessionStore(source: Reducer())
        let turn = store.turn
        store.apply(ChangeBatch(
            revision: 1, retainedBytes: 0, seq: 1, changes: [.added(index: 0, item: item)],
            turn: turn, streaming: nil, title: "original"))
        store.apply(ChangeBatch(
            revision: 2, retainedBytes: 0, seq: 99, changes: [.error(message: "discarded")] + changes,
            turn: reducer.turn(), streaming: StreamId(call: 123, attempt: 0), title: "discarded"))
        #expect(store.items == [item])
        #expect(store.rev == 1 && store.seq == 1)
        #expect(store.title == "original")
        #expect(store.turn == turn && store.streaming == nil)
        #expect(store.sessionId == nil && store.workspace.isEmpty && store.problem == nil)

        store.apply(ChangeBatch(
            revision: 2, retainedBytes: 0, seq: 2,
            changes: [.added(index: 1, item: next), .updated(index: 1, item: next)],
            turn: turn, streaming: nil, title: "retry"))
        #expect(store.items == [item, next])
        #expect(store.rev == 2 && store.seq == 2 && store.title == "retry")
    }
}

@Test func aRetryReplacesStreamedTextAtEveryLength() throws {
    let vector = Vector.all.first { $0.name == "retry replaces streamed text at every length" }!
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    for message in vector.messages {
        if let update = try reducer.fold(message: message) { store.apply(update) }
        expectInStep(store, reducer, vector.name)
    }
    #expect(store.streaming?.text == "€")
}

/// Delays a real resync result after Rust has built it, without holding its feed lock.
nonisolated private final class DelayedRecovery: ViewSource, Sendable {
    let reducer: Reducer
    let release = DispatchSemaphore(value: 0)
    let waiting = Mutex(false)

    init(_ reducer: Reducer) { self.reducer = reducer }

    func updates() -> AsyncStream<ChangeBatch> { reducer.updates() }

    func resync(from: UInt64) -> Resync {
        let result = reducer.resync(from: from)
        waiting.withLock { $0 = true }
        release.wait()
        return result
    }
}

@Test func anOlderAsyncSnapshotNeverReplacesSubsequentUpdates() async throws {
    let vector = Vector.all.first { $0.name == "a far behind renderer takes a snapshot" }!
    let (reducer, updates) = try vector.fold()
    let source = DelayedRecovery(reducer)
    defer { source.release.signal() }
    let store = SessionStore(source: source)
    store.apply(updates.last!)
    // Reaching this condition on MainActor while resync is blocked proves it stays responsive.
    try await eventually("snapshot built off MainActor") { source.waiting.withLock { $0 } }
    let recovery = store.recovery
    for update in updates { store.apply(update) }
    let newer = try #require(try reducer.fold(message: vector.messages.last!))
    store.apply(newer)
    source.release.signal()
    await recovery?.value
    #expect(store.rev == newer.revision)
    expectInStep(store, reducer, "snapshot racing newer updates")
}

@Test func detachingDuringAsyncRecoveryNeverResurrectsAnAttachment() async throws {
    let vector = Vector.all.first { $0.name == "a far behind renderer takes a snapshot" }!
    let (reducer, updates) = try vector.fold()
    let source = DelayedRecovery(reducer)
    defer { source.release.signal() }
    let store = SessionStore(source: source)
    store.apply(updates.last!)
    try await eventually("snapshot in flight") { source.waiting.withLock { $0 } }
    let recovery = store.recovery
    await store.detach()
    source.release.signal()
    await recovery?.value
    #expect(store.attachment == .detached)
    #expect(store.link == .detached && store.rev == 0 && store.items.isEmpty)
}

@Test func updatesArrivingDuringRecoveryAreRecoveredWithoutAnotherNotification() async throws {
    let vector = Vector.all.first { $0.name == "a far behind renderer takes a snapshot" }!
    let (reducer, updates) = try vector.fold()
    let source = DelayedRecovery(reducer)
    defer { source.release.signal() }
    let store = SessionStore(source: source)
    store.apply(updates.last!)
    try await eventually("first recovery captured") { source.waiting.withLock { $0 } }
    let newer = try #require(try reducer.fold(message: vector.messages.last!))
    store.apply(newer)
    // One permit for the captured snapshot, one for the suffix requested afterwards.
    source.release.signal()
    source.release.signal()
    try await eventually("latest revision recovered") { store.rev == newer.revision }
    #expect(store.resyncs == 2)
    expectInStep(store, reducer, "update raced snapshot")
}

private func inputFrame(seq: UInt64, text: String) throws -> String {
    let vector = Vector.all.first { $0.name == "a far behind renderer takes a snapshot" }!
    var frame = try JSONSerialization.jsonObject(with: Data(vector.messages[1].utf8)) as! [String: Any]
    frame["seq"] = seq
    frame["segments"] = [["text": text]]
    return String(decoding: try JSONSerialization.data(withJSONObject: frame), as: UTF8.self)
}

@Test func largeItemsRecoverWithoutARevisionGap() async throws {
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    let text = String(repeating: "x", count: 1 << 20)
    let update = try #require(try reducer.fold(message: inputFrame(seq: 1, text: text)))
    #expect(update.revision == 1)
    store.apply(update)
    await store.recovery?.value
    #expect(store.rev == 1)
    #expect(store.items.first?.presentation.body?.text == text)
    expectInStep(store, reducer, "large contiguous item")
}

@Test func anAsyncPatchNeverAppliesToAChangedBase() async throws {
    let reducer = Reducer()
    let first = try #require(try reducer.fold(message: inputFrame(seq: 1, text: "first")))
    let second = try #require(try reducer.fold(message: inputFrame(seq: 2, text: String(repeating: "x", count: 1 << 20))))
    let source = DelayedRecovery(reducer)
    defer { source.release.signal() }
    let store = SessionStore(source: source)
    store.apply(second)
    try await eventually("patch captured against empty base") { source.waiting.withLock { $0 } }
    store.apply(first)
    source.release.signal()
    source.release.signal()
    try await eventually("patch rebuilt against the advanced base") { store.rev == second.revision }
    #expect(store.items.count == 2 && store.resyncs == 2)
    expectInStep(store, reducer, "changed patch base")
}

@Test func largeStreamsRecoverCompleteTextWithoutReplacingItems() async throws {
    let vector = Vector.all.first { $0.name == "retry replaces streamed text at every length" }!
    let reducer = Reducer()
    let store = SessionStore(source: reducer)
    for message in vector.messages.prefix(3) {
        if let update = try reducer.fold(message: message) { store.apply(update) }
    }
    let items = store.items
    var frame = try JSONSerialization.jsonObject(with: Data(vector.messages[3].utf8)) as! [String: Any]
    let text = String(repeating: "x", count: 1 << 20)
    frame["text"] = text
    let message = String(decoding: try JSONSerialization.data(withJSONObject: frame), as: UTF8.self)
    let update = try #require(try reducer.fold(message: message))
    store.apply(update)
    await store.recovery?.value
    #expect(store.streaming?.text == text)
    #expect(store.items == items)
    expectInStep(store, reducer, "large stream patch")
}
