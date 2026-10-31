import Foundation
import Observation

extension ConversationWatch: @retroactive @unchecked Sendable {}
nonisolated extension ConversationInfo: @retroactive Identifiable {}

@Observable
public final class ConversationStore {
    public let origin: String
    public private(set) var rows: [ConversationInfo] = []
    public private(set) var problem: String?
    @ObservationIgnored private let watch: ConversationWatch
    @ObservationIgnored private var task: Task<Void, Never>?

    public init(client: Client, origin: String) throws {
        self.origin = origin
        watch = try ConversationWatch(client: client, origin: origin)
    }

    public func start() {
        guard task == nil else { return }
        let updates = watch.updates()
        watch.start()
        task = Task { [weak self] in
            for await rows in updates {
                guard let self else { return }
                self.rows = rows
            }
        }
    }

    public func stop() {
        task?.cancel(); task = nil
    }

    public func interrupt(_ id: String) {
        do { try watch.interrupt(session: id) }
        catch { problem = error.localizedDescription }
    }

    deinit { task?.cancel() }
}
