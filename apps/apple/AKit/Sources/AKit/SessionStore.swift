import Foundation
import Observation

public enum CacheState: Equatable {
    case active, cached, detached
}

public enum AttachmentState: Equatable {
    case detached
    case attaching
    case attached(id: String)
}

/// A session view follows numbered Rust updates; gaps require resynchronization.
@Observable
public final class SessionStore {
    public private(set) var items: [DisplayItem] = []
    /// Live text of the reply in flight.
    public private(set) var streaming: StreamingText?
    public private(set) var recap: RecapInfo?
    public private(set) var link: LinkState = .connecting
    /// What the session's turn is doing and what the controls offer.
    public private(set) var turn = TurnInfo(
        running: false, asking: nil, asks: 0, canInterrupt: false, canSend: false)
    public private(set) var sessionId: String?
    public private(set) var workspace = ""
    /// The session's title, once its model named it.
    public private(set) var title: String?
    /// The last update applied.
    public private(set) var rev: UInt64 = 0
    public private(set) var seq: UInt64 = 0
    /// The latest error the daemon reported, or the reason the link closed.
    public var problem: String?
    /// A newer `a` the daemon runs, when this build cannot be served.
    public private(set) var newerBuild: String?
    /// How many gaps this store has caught up on.
    public private(set) var resyncs = 0

    public private(set) var attachment: AttachmentState = .detached {
        didSet {
            if attachment != oldValue { attachmentChanged?(self) }
        }
    }

    public fileprivate(set) var cacheState: CacheState = .detached
    fileprivate var retainedBytes: UInt64 { session?.retainedBytes() ?? 0 }
    @ObservationIgnored fileprivate var viewChanged: (() -> Void)?

    @ObservationIgnored private var source: (any ViewSource)?
    @ObservationIgnored private var session: Session?
    @ObservationIgnored fileprivate var attachmentChanged: ((SessionStore) -> Void)?
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

    /// Starts following the source's updates.
    public func listen() {
        guard listening == nil, let source else { return }
        let updates = source.updates()
        listening = Task { [weak self] in
            var batch = 0
            for await update in updates {
                guard !Task.isCancelled else { break }
                self?.apply(update)
                batch += 1
                if batch == 64 {
                    batch = 0
                    await Task.yield()
                }
            }
        }
    }

    /// Follows the session, then attaches to `target`.
    public func open(_ target: OpenTarget) {
        guard let session, attachment == .detached else { return }
        attachment = .attaching
        listen()
        do {
            try session.open(target: target)
        } catch {
            problem = error.localizedDescription
        }
    }

    public func send(_ text: String) async {
        run { try $0.send(text: text) }
    }

    /// A line typed, parsed by the commands every client shares; what it
    /// asks of the app comes back.
    public func submit(_ line: String) async -> AppAction? {
        guard let session else { return nil }
        do {
            return try session.submit(line: line)
        } catch {
            problem = error.localizedDescription
            return nil
        }
    }

    /// Every slash command, for help and completion.
    public var commands: [CommandHelp] { session?.commands() ?? [] }

    /// Replaces a daemon that stopped answering, and attaches again.
    public func restart() async {
        run { try $0.restart() }
    }

    public func approve(call: UInt64, approved: Bool) async {
        run { try $0.approve(call: call, approved: approved) }
    }

    public func interrupt() async {
        run { try $0.interrupt() }
    }

    public func detach() async {
        let session = self.session
        releaseAttachment()
        do {
            try session?.detach()
        } catch {
            problem = error.localizedDescription
        }
    }

    fileprivate func releaseAttachment() {
        cacheState = .detached
        listening?.cancel()
        listening = nil
        session = nil
        source = nil
        link = .detached
        attachment = .detached
    }

    private func run(_ action: (Session) throws -> Void) {
        guard let session else { return }
        do {
            try action(session)
        } catch {
            problem = error.localizedDescription
        }
    }

    /// Applies `update`: once, in order, catching up first on a gap.
    public func apply(_ update: ChangeBatch) {
        if update.revision <= rev { return }
        defer { viewChanged?() }
        if update.revision != rev + 1 {
            catchUp()
            if update.revision <= rev { return }
        }
        fold(update)
    }

    private func catchUp() {
        guard let source else { return }
        resyncs += 1
        switch source.resync(from: rev) {
        case .updates(let updates):
            for update in updates where update.revision == rev + 1 {
                fold(update)
            }
        case .snapshot(let snapshot):
            adopt(snapshot)
        }
    }

    private func adopt(_ s: SessionSnapshot) {
        (rev, seq, sessionId, workspace, title) = (s.rev, s.seq, s.session, s.workspace, s.title)
        (items, link, recap, streaming, turn) = (s.items, s.link, s.recap, s.streaming, s.turn)
        if let id = s.session { attachment = .attached(id: id) }
        if s.link == .detached { releaseAttachment() }
    }

    private func fold(_ update: ChangeBatch) {
        // Preflight keeps invalid batches inert without copying the transcript.
        var count = items.count
        for change in update.changes {
            switch change {
            case .attached(_, _, let reset) where reset:
                count = 0
            case .snapshot(let items):
                count = items.count
            case .added(let index, _):
                guard Int(index) <= count else { return }
                count += 1
            case .updated(let index, _):
                guard Int(index) < count else { return }
            case .attached, .delta, .synced, .activity, .draining, .mismatch, .error, .link:
                break
            }
        }
        (rev, seq, turn, title) = (update.revision, update.seq, update.turn, update.title)
        if streaming?.call != update.streaming {
            streaming = update.streaming.map { StreamingText(call: $0, text: "") }
        }
        for change in update.changes {
            switch change {
            case .attached(let session, let workspace, let reset):
                if reset {
                    (items, recap) = ([], nil)
                }
                (sessionId, self.workspace) = (session, workspace)
                attachment = .attached(id: session)
            case .added(let index, let item):
                items.insert(item, at: Int(index))
            case .updated(let index, let item):
                items[Int(index)] = item
            case .delta(_, let text):
                streaming?.text += text
            case .synced(let recap):
                self.recap = recap
            case .snapshot(let snapshot):
                items = snapshot
            case .activity, .draining:
                break
            case .mismatch(_, let message):
                problem = message
            case .error(let message):
                problem = message
            case .link(let state):
                link = state
                if state == .detached { releaseAttachment() }
                if case .lost(let message, let newer) = state {
                    (problem, newerBuild) = (message, newer)
                }
            }
        }
    }
}

/// Cache keys and selection track the identity reported by the live attachment.
@Observable
public final class SessionStoreCache {
    public private(set) var stores: [String: SessionStore] = [:]
    public var selection: String? {
        didSet {
            if let selection { order.removeAll { $0 == selection }; order.append(selection) }
            trim()
        }
    }
    @ObservationIgnored private var order: [String] = []
    private let budgetBytes: UInt64
    private let capacity: Int

    public init(capacity: Int = 4, bytes: UInt64 = 16 << 20) {
        self.capacity = max(0, capacity)
        self.budgetBytes = bytes
    }

    private func trim() {
        for (id, store) in stores { store.cacheState = id == selection ? .active : .cached }
        var inactive = order.filter { $0 != selection && stores[$0] != nil }
        var bytes = inactive.reduce(UInt64(0)) { $0 + (stores[$1]?.retainedBytes ?? 0) }
        while inactive.count > capacity || bytes > budgetBytes {
            let id = inactive.removeFirst()
            guard let store = stores.removeValue(forKey: id) else { continue }
            bytes -= store.retainedBytes
            order.removeAll { $0 == id }
            store.attachmentChanged = nil
            store.viewChanged = nil
            store.releaseAttachment()
        }
    }

    @discardableResult
    public func open(_ id: String, client: Client) -> SessionStore {
        if let store = stores[id] { return store }
        let store = SessionStore(session: Session(client: client))
        insert(store, id: id)
        store.open(.id(id: id))
        return store
    }

    public func insert(_ store: SessionStore, id: String) {
        stores[id] = store
        order.removeAll { $0 == id }
        order.append(id)
        store.viewChanged = { [weak self] in self?.trim() }
        store.attachmentChanged = { [weak self] store in self?.rekey(store) }
        if case .attached = store.attachment { rekey(store) }
        trim()
    }

    private func rekey(_ store: SessionStore) {
        if case .attaching = store.attachment { return }
        let oldKeys = stores.keys.filter { stores[$0] === store }
        let selected = oldKeys.contains { $0 == selection }
        for key in oldKeys { stores.removeValue(forKey: key) }
        order.removeAll { oldKeys.contains($0) }
        if case .attached(let id) = store.attachment {
            stores[id] = store
            order.append(id)
            if selected { selection = id }
        } else if selected {
            selection = nil
        }
        trim()
    }
}

/// The daemon's sessions, newest first, as the daemon pushes them over one
/// connection whenever one changes.
@Observable
public final class SessionListStore {
    public private(set) var sessions: [SessionInfo] = []
    public var problem: String?

    @ObservationIgnored private let watch: SessionWatch

    public init(client: Client) {
        self.watch = SessionWatch(client: client)
    }

    /// Follows the list until the task is cancelled, calling `changed`
    /// after each update.
    public func follow(_ changed: () -> Void = {}) async {
        let updates = watch.updates()
        watch.start()
        for await update in updates {
            if let problem = update.problem {
                self.problem = problem
            } else {
                (sessions, problem) = (update.sessions, nil)
            }
            changed()
        }
    }
}

/// Staging and login-shell environment acquisition can wait on disk or a process.
public nonisolated func startClient(helper: String) async throws -> Client {
    try await stageClient { try Client(helper: helper) }
}

nonisolated func stageClient<T: Sendable>(
    _ stage: @escaping @Sendable () throws -> T
) async throws -> T {
    try await Task.detached(operation: stage).value
}
