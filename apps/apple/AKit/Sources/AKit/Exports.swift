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

    /// This item with `patch` applied: the rest of it stands. A patch for
    /// another kind of item cannot be made, so one is left as it is.
    func patched(_ patch: ItemPatch) -> ViewItem {
        switch (self, patch) {
        case (
            .tool(let call, let tool, let input, _, let approval, _, _, _),
            .outcome(let outcome, let title, let card, let phase)
        ):
            .tool(
                call: call, tool: tool, inputJson: input, title: title, approval: approval,
                outcome: outcome, card: card, phase: phase)
        case (.tool(let call, let tool, let input, let title, _, let outcome, let card, _), .approval(let approval, let phase)):
            .tool(
                call: call, tool: tool, inputJson: input, title: title, approval: approval,
                outcome: outcome, card: card, phase: phase)
        case (_, .command(let item)):
            item
        case (.child(let call, let child, let input, let watch, _), .outcome(let fired, _, _, _)):
            .child(call: call, child: child, input: input, watch: watch, fired: fired)
        case (.child(let call, let child, let input, _, let fired), .watch(let watch)):
            .child(call: call, child: child, input: input, watch: watch, fired: fired)
        case (.reply(let call, let model, _, _, _, let purpose), .reply(let text, let status, let usage)):
            .reply(call: call, model: model, text: text, status: status, usage: usage, purpose: purpose)
        default:
            self
        }
    }
}

nonisolated extension SessionInfo: @retroactive Identifiable {}

extension ClientError: @retroactive LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .unavailable(let message), .notResponding(let message), .mismatch(let message, _),
            .protocol(let message), .notOffered(let message), .invalidArgument(let message):
            message
        case .closed: "The connection to the daemon closed."
        case .notOpen: "The session is not open."
        }
    }
}
