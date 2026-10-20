import Foundation
import Observation

/// One session as the app shows it: a copy of the Rust session client's
/// view, kept in step by its numbered updates. It holds no rule of its own:
/// the turn (what runs, what waits, what the controls offer), each call's
/// phase and the link come computed from Rust. The stream drops updates
/// when its buffer is full, so a `rev` gap asks the source for what was
/// missed (or a snapshot) before going on.
@Observable
public final class SessionStore {
    public private(set) var items: [ViewItem] = []
    /// Live text of the reply in flight.
    public private(set) var streaming: StreamingText?
    public private(set) var recap: RecapInfo?
    public private(set) var link: LinkState = .connecting
    /// What the session's turn is doing and what the controls offer.
    public private(set) var turn = TurnInfo(
        running: false, asking: nil, asks: 0, canInterrupt: false, canSteer: false, canSend: false)
    public private(set) var sessionId: String?
    public private(set) var workspace = ""
    /// The session's title, once its model named it.
    public private(set) var title: String?
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
    public func open(_ target: OpenTarget) {
        listen()
        do {
            try session?.open(target: target)
        } catch {
            problem = error.localizedDescription
        }
    }

    public func send(_ text: String) async {
        await run { try await $0.send(text: text) }
    }

    /// A line typed, parsed by the commands every client shares; what it
    /// asks of the app comes back.
    public func submit(_ line: String) async -> AppAction? {
        guard let session else { return nil }
        do {
            return try await session.submit(line: line)
        } catch {
            problem = error.localizedDescription
            return nil
        }
    }

    /// Every slash command, for help and completion.
    public var commands: [CommandHelp] { session?.commands() ?? [] }

    /// Replaces a daemon that stopped answering, and attaches again.
    public func restart() async {
        await run { try await $0.restart() }
    }

    public func approve(call: UInt64, approved: Bool) async {
        await run { try await $0.approve(call: call, approved: approved) }
    }

    public func interrupt() async {
        await run { try await $0.interrupt() }
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
        (rev, seq, sessionId, workspace, title) = (s.rev, s.seq, s.session, s.workspace, s.title)
        (items, link, recap, streaming, turn) = (s.items, s.link, s.recap, s.streaming, s.turn)
    }

    private func fold(_ update: ViewUpdate) {
        (rev, seq, turn, title) = (update.rev, update.seq, update.turn, update.title)
        switch update.change {
        case .attached(let session, let workspace, let reset):
            if reset {
                (items, recap, streaming) = ([], nil, nil)
            }
            (sessionId, self.workspace) = (session, workspace)
        case .added(let index, let item):
            items.insert(item, at: min(Int(index), items.count))
        case .updated(let index, let item, let part):
            guard Int(index) < items.count else { return }
            items[Int(index)] = item
            if part == .outcome, case .reply(let call, _, _, _, _, _) = item, streaming?.call == call {
                streaming = nil
            }
        case .delta(let call, let text):
            if streaming?.call == call {
                streaming?.text += text
            } else {
                streaming = StreamingText(call: call, text: text)
            }
        case .synced(let recap):
            self.recap = recap
        case .snapshot(let snapshot):
            items = snapshot
            if let call = streaming?.call, !items.contains(where: {
                if case .reply(call, _, _, .running, _, _) = $0 { true } else { false }
            }) {
                streaming = nil
            }
        case .draining:
            break
        case .mismatch(let supported):
            problem = "The daemon speaks protocol \(supported); restart it."
        case .error(let message):
            problem = message
        case .link(let state):
            link = state
            if case .lost(let message) = state { problem = message }
        }
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
