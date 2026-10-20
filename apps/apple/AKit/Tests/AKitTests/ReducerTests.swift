import Foundation
import Testing

@testable import AKit

/// The shared reducer vectors (crates/protocol/tests/vectors/view.json),
/// run through the FFI: every client folds them to the same state.
struct Vector {
    let name: String
    let messages: [String]
    let updates: [(rev: UInt64, seq: UInt64)]
    let state: NSDictionary
    let hash: String

    static let all: [Vector] = {
        let url = URL(filePath: #filePath)
            .deletingLastPathComponent()
            .appending(path: "../../../../../crates/protocol/tests/vectors/view.json")
            .standardized
        let data = try! Data(contentsOf: url)
        let vectors = try! JSONSerialization.jsonObject(with: data) as! [[String: Any]]
        return vectors.map { v in
            let messages = (v["messages"] as! [Any]).map {
                String(decoding: try! JSONSerialization.data(withJSONObject: $0), as: UTF8.self)
            }
            let updates = (v["updates"] as! [[String: Any]]).map {
                (($0["rev"] as! NSNumber).uint64Value, ($0["seq"] as! NSNumber).uint64Value)
            }
            return Vector(
                name: v["name"] as! String, messages: messages, updates: updates,
                state: v["state"] as! NSDictionary, hash: v["hash"] as! String)
        }
    }()

    /// Folds the messages into a fresh reducer.
    func fold() throws -> (Reducer, [ViewUpdate]) {
        let reducer = Reducer()
        let updates = try messages.compactMap { try reducer.fold(message: $0) }
        return (reducer, updates)
    }
}

@Test func theVectorsFoldToTheSameStateAndHash() throws {
    #expect(!Vector.all.isEmpty)
    for vector in Vector.all {
        let (reducer, updates) = try vector.fold()
        #expect(updates.map(\.rev) == vector.updates.map(\.rev), "\(vector.name)")
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
    var last: ViewUpdate?
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
    for message in [
        #"{"t":"attached","session":"00000000-0000-0000-0000-00000000000a","workspace":"/w"}"#,
        #"{"t":"entry","seq":1,"epoch":0,"kind":"tool_call","tool":"bash","input":{"command":"ls"}}"#,
        #"{"t":"entry","seq":2,"epoch":0,"kind":"decision","by":"policy","call":1,"d":"permission","permission":"ask"}"#,
    ] {
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
    guard case .reply(_, _, _, _, _, let purpose) = store.items.first else {
        Issue.record("a reply item")
        return
    }
    #expect(purpose == "title")
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
