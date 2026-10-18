import Foundation
import Observation

/// One session as the app shows it: a copy of the Rust view, kept in step
/// by its numbered updates. The stream drops updates when its buffer is
/// full, so a `rev` gap asks the source for what was missed (or a snapshot)
/// before going on.
@Observable
public final class SessionStore {
    public private(set) var items: [ViewItem] = []
    /// Live text of the reply in flight.
    public private(set) var streaming: StreamingText?
    public private(set) var recap: RecapInfo?
    public private(set) var link: LinkState = .connecting
    public private(set) var sessionId: String?
    public private(set) var workspace = ""
    /// The last update applied.
    public private(set) var rev: UInt64 = 0
    public private(set) var seq: UInt64 = 0
    /// The latest error the daemon reported, or the reason the link closed.
    public var problem: String?
    /// How many gaps this store has caught up on.
    public private(set) var resyncs = 0

    @ObservationIgnored private let source: any ViewSource
    @ObservationIgnored private let session: Session?
    @ObservationIgnored private var listening: Task<Void, Never>?

    /// A store for `session`; `open` attaches it.
    public init(session: Session) {
        self.source = session
        self.session = session
    }

    /// A store following any source, such as a reducer fed by hand.
    public init(source: any ViewSource) {
        self.source = source
        self.session = nil
    }

    /// Calls this client was asked to approve that still wait, oldest first.
    public var pendingAsks: [ViewItem] {
        items.filter {
            if case .tool(_, _, _, _, .pending?, _, _, true) = $0 { true } else { false }
        }
    }

    /// Starts following the source's updates.
    public func listen() {
        guard listening == nil else { return }
        let updates = source.updates()
        listening = Task { [weak self] in
            for await update in updates {
                self?.apply(update)
            }
        }
    }

    /// Follows the session, then attaches to `target`.
    public func open(_ target: OpenTarget) async {
        listen()
        await run { try await $0.open(target: target) }
    }

    public func send(_ text: String) async {
        await run { try await $0.send(text: text) }
    }

    public func approve(call: UInt64, approved: Bool) async {
        await run { try await $0.approve(call: call, approved: approved) }
    }

    public func detach() async {
        await run { try await $0.detach() }
        listening?.cancel()
        listening = nil
    }

    private func run(_ action: (Session) async throws -> Void) async {
        guard let session else { return }
        do {
            try await action(session)
        } catch {
            problem = error.localizedDescription
        }
    }

    /// Applies `update`: once, in order, catching up first on a gap.
    public func apply(_ update: ViewUpdate) {
        if update.rev <= rev { return }
        if update.rev != rev + 1 {
            catchUp()
            if update.rev <= rev { return }
        }
        fold(update)
    }

    private func catchUp() {
        resyncs += 1
        switch source.resync(from: rev) {
        case .updates(let updates):
            for update in updates where update.rev == rev + 1 {
                fold(update)
            }
        case .snapshot(let snapshot):
            adopt(snapshot)
        }
    }

    private func adopt(_ s: SessionSnapshot) {
        (rev, seq, sessionId, workspace) = (s.rev, s.seq, s.session, s.workspace)
        (items, link, recap, streaming) = (s.items, s.link, s.recap, s.streaming)
    }

    private func fold(_ update: ViewUpdate) {
        (rev, seq) = (update.rev, update.seq)
        switch update.change {
        case .attached(let session, let workspace, let reset):
            if reset {
                (items, recap, streaming) = ([], nil, nil)
            }
            (sessionId, self.workspace, link) = (session, workspace, .replaying)
        case .added(let index, let item):
            items.insert(item, at: min(Int(index), items.count))
        case .updated(let index, let item, let part):
            guard Int(index) < items.count else { return }
            items[Int(index)] = item
            if part == .outcome, case .reply(let call, _, _, _, _) = item, streaming?.call == call {
                streaming = nil
            }
        case .asked(let index, let item):
            let at = Int(index)
            if at < items.count, items[at].key() == item.key() {
                items[at] = item
            } else {
                items.insert(item, at: min(at, items.count))
            }
        case .delta(let call, let text):
            if streaming?.call == call {
                streaming?.text += text
            } else {
                streaming = StreamingText(call: call, text: text)
            }
        case .synced:
            link = .live
        case .snapshot(let snapshot):
            items = snapshot
            if let call = streaming?.call, !items.contains(where: {
                if case .reply(call, _, _, .running, _) = $0 { true } else { false }
            }) {
                streaming = nil
            }
        case .recap(let recap):
            self.recap = recap
        case .draining:
            link = .draining
        case .mismatch(let supported):
            problem = "The daemon speaks protocol \(supported); restart it."
        case .error(let message):
            problem = message
        case .closed(let message):
            link = .closed
            problem = message
        case .detached:
            link = .closed
        }
    }
}

/// The daemon's sessions.
@Observable
public final class SessionListStore {
    public private(set) var sessions: [SessionInfo] = []
    public var problem: String?

    @ObservationIgnored private let client: Client

    public init(client: Client) {
        self.client = client
    }

    public func refresh() async {
        do {
            sessions = try await client.sessions().reversed()
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }
}
