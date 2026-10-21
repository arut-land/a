@_exported import AFfi
import Foundation

// The Rust objects are Send + Sync: every method locks what it touches.
extension Client: @retroactive @unchecked Sendable {}
extension Session: @retroactive @unchecked Sendable {}
extension Reducer: @retroactive @unchecked Sendable {}
extension SessionWatch: @retroactive @unchecked Sendable {}

/// Where a store's updates come from, and where it catches up after a gap:
/// a daemon session, or a reducer fed by hand.
public protocol ViewSource: AnyObject, Sendable {
    nonisolated func updates() -> AsyncStream<ViewUpdate>
    nonisolated func resync(from: UInt64) -> Resync
}

extension Session: ViewSource {}
extension Reducer: ViewSource {}

nonisolated extension ViewItem: @retroactive Identifiable {
    public var id: UInt64 { key() }

    /// This item with `patch` applied: the rest of it stands. Each patch
    /// brings how the item is shown now; a patch for another kind of item
    /// cannot be made, so one is left as it is.
    func patched(_ patch: ItemPatch) -> ViewItem {
        switch (self, patch) {
        case (.tool(let call, let tool, let input, _, _), .outcome(let outcome, let shown)):
            .tool(call: call, tool: tool, inputJson: input, outcome: outcome, shown: shown)
        case (.tool(let call, let tool, let input, let outcome, _), .phase(let shown)),
            (.tool(let call, let tool, let input, let outcome, _), .answer(_, let shown)):
            .tool(call: call, tool: tool, inputJson: input, outcome: outcome, shown: shown)
        case (_, .command(let item)):
            item
        case (.child(let call, let child, let input, let watch, _, _), .outcome(let fired, let shown)):
            .child(call: call, child: child, input: input, watch: watch, fired: fired, shown: shown)
        case (.child(let call, let child, let input, _, let fired, let shown), .watch(let watch)):
            .child(call: call, child: child, input: input, watch: watch, fired: fired, shown: shown)
        case (.reply(let call, let model, _, _, let purpose, _), .reply(let text, let status, let shown)):
            .reply(call: call, model: model, text: text, status: status, purpose: purpose, shown: shown)
        default:
            self
        }
    }
}

nonisolated extension SessionInfo: @retroactive Identifiable {}

extension ClientError: @retroactive LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .busy(let message), .unavailable(let message), .notResponding(let message), .mismatch(let message, _),
            .protocol(let message), .notOffered(let message), .invalidArgument(let message):
            message
        case .closed: "The connection to the daemon closed."
        case .notOpen: "The session is not open."
        }
    }
}
