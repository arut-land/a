@_exported import AFfi
import Foundation

// The Rust objects are Send + Sync: every method locks what it touches.
extension Client: @retroactive @unchecked Sendable {}
extension Session: @retroactive @unchecked Sendable {}
extension Reducer: @retroactive @unchecked Sendable {}
extension SessionWatch: @retroactive @unchecked Sendable {}
extension QueryStream: @retroactive @unchecked Sendable {}

/// Where a store's updates come from, and where it catches up after a gap:
/// a daemon session, or a reducer fed by hand.
public protocol ViewSource: AnyObject, Sendable {
    nonisolated func updates() -> AsyncStream<ChangeBatch>
    nonisolated func resync(from: UInt64) -> Resync
}

extension Session: ViewSource {}
extension Reducer: ViewSource {}

nonisolated extension DisplayItem: @retroactive Identifiable {
    public var id: UInt64 { key }
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
