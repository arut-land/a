import Foundation
import Testing

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
    #expect(store.turn == snapshot.turn, "\(name)")
}

@Test func aStoreAppliesTheUpdatesToTheSameView() throws {
    for vector in Vector.all {
        let (reducer, updates) = try vector.fold()
        let store = SessionStore(source: reducer)
        updates.forEach(store.apply)
        // Repeats change nothing.
        updates.forEach(store.apply)
        expectInStep(store, reducer, vector.name)
        #expect(store.resyncs == 0)
    }
}

@Test func aStoreCatchesUpOnDroppedUpdates() throws {
    for vector in Vector.all {
        let (reducer, updates) = try vector.fold()
        let store = SessionStore(source: reducer)
        for (i, update) in updates.enumerated() where i % 3 != 1 {
            store.apply(update)
        }
        expectInStep(store, reducer, vector.name)
        #expect(updates.count < 3 || store.resyncs > 0, "\(vector.name)")
    }
}

@Test func aStoreFarBehindTakesASnapshot() throws {
    let reducer = Reducer()
    _ = try reducer.fold(
        message: #"{"t":"attached","session":"00000000-0000-0000-0000-00000000000a","workspace":"/w"}"#)
    let store = SessionStore(source: reducer)
    var last: ChangeBatch?
    for seq in 1...1000 {
        last = try reducer.fold(
            message: #"{"t":"entry","seq":\#(seq),"epoch":0,"kind":"input","text":"m\#(seq)","taken":true}"#)
    }
    store.apply(last!)
    expectInStep(store, reducer, "far behind")
    #expect(store.items.count == 1000 && store.resyncs == 1)
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
    for message in [
        #"{"t":"attached","session":"00000000-0000-0000-0000-00000000000a","workspace":"/w"}"#,
        #"{"t":"entry","seq":1,"epoch":0,"kind":"model_call","model":"m","purpose":"title"}"#,
        #"{"t":"entry","seq":2,"epoch":0,"kind":"model_result","call":1,"outcome":{"status":"done","output":{"message":{"content":[{"type":"text","text":"Fix the parser"}]}}}}"#,
    ] {
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
        revision: 1, seq: 99,
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
        [.snapshot(items: []), .updated(index: 0, item: item)],
        [.attached(session: "discarded", workspace: "/discarded", reset: true), .updated(index: 0, item: item)],
        [.added(index: 1, item: next), .updated(index: 2, item: item)],
    ]
    for changes in invalid {
        let store = SessionStore(source: Reducer())
        let turn = store.turn
        store.apply(ChangeBatch(
            revision: 1, seq: 1, changes: [.added(index: 0, item: item)],
            turn: turn, streaming: nil, title: "original"))
        store.apply(ChangeBatch(
            revision: 2, seq: 99, changes: [.error(message: "discarded")] + changes,
            turn: reducer.turn(), streaming: 123, title: "discarded"))
        #expect(store.items == [item])
        #expect(store.rev == 1 && store.seq == 1)
        #expect(store.title == "original")
        #expect(store.turn == turn && store.streaming == nil)
        #expect(store.sessionId == nil && store.workspace.isEmpty && store.problem == nil)

        store.apply(ChangeBatch(
            revision: 2, seq: 2,
            changes: [.added(index: 1, item: next), .updated(index: 1, item: next)],
            turn: turn, streaming: nil, title: "retry"))
        #expect(store.items == [item, next])
        #expect(store.rev == 2 && store.seq == 2 && store.title == "retry")
    }
}
