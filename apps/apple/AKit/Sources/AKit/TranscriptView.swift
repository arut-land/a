import AppKit
import SwiftUI
import os

/// Native scrolling retains a visible row while adjacent history pages arrive.
public struct TranscriptView<Row: View>: View {
    private let store: SessionStore
    private let row: (DisplayItem) -> Row

    public init(store: SessionStore, @ViewBuilder row: @escaping (DisplayItem) -> Row) {
        self.store = store
        self.row = row
    }

    public var body: some View {
        TranscriptScroll(store: store, items: store.items, history: store.history,
                         follows: store.followsHistory, row: row)
    }
}

private struct TranscriptEntry: Identifiable, Equatable {
    struct ID: Hashable {
        let key: UInt64
        let offset: UInt64
    }
    let item: DisplayItem
    let id: ID
}

private struct TranscriptScroll<Row: View>: NSViewRepresentable {
    let store: SessionStore
    let items: [DisplayItem]
    let history: HistoryWindow?
    let follows: Bool
    let row: (DisplayItem) -> Row

    func makeCoordinator() -> TranscriptCoordinator { TranscriptCoordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = TranscriptScroller()
        scroll.owner = context.coordinator
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.contentView.postsBoundsChangedNotifications = true
        context.coordinator.connect(scroll)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        let entries = coordinator.install(items, history: history, follows: follows)
        coordinator.store = store
        coordinator.content = { [weak coordinator] width in
            AnyView(VStack(alignment: .leading, spacing: 10) {
                if let history, history.start > 0 || history.textStart > 0 {
                    Button("Earlier messages") { store.loadEarlier() }.disabled(history.pending)
                }
                ForEach(entries) { entry in
                    row(entry.item)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background {
                            if let coordinator { TranscriptMarker(id: entry.id, coordinator: coordinator) }
                        }
                        .id(entry.id)
                }
                if !follows {
                    HStack {
                        Button("Later messages") { store.loadLater() }.disabled(history?.pending == true)
                        Button("Latest messages") { store.followLatest() }
                    }
                }
            }.padding(16).frame(width: width, alignment: .topLeading))
        }
        coordinator.update()
    }
}

private final class TranscriptCoordinator: NSObject {
    private var page: [TranscriptEntry] = []
    private var previous: [TranscriptEntry] = []
    private var generation: UInt32?
    private var start: UInt64?
    private var textStart: UInt64?
    private var follows = true
    private var pinned = true
    private var updating = false
    private var layoutPending = false
    private var lastOrigin: NSPoint = .zero
    private var width: CGFloat = 0
    private weak var scroll: NSScrollView?
    private let host = TranscriptHost(rootView: AnyView(EmptyView()))
    fileprivate var markers: [TranscriptEntry.ID: NSView] = [:]
    var store: SessionStore?
    var content: ((CGFloat) -> AnyView)?

    func connect(_ scroll: NSScrollView) {
        self.scroll = scroll
        scroll.documentView = host
        host.invalidated = { [weak self] in self?.scheduleLayout() }
        NotificationCenter.default.addObserver(self, selector: #selector(scrolled),
            name: NSView.boundsDidChangeNotification, object: scroll.contentView)
    }

    func install(_ items: [DisplayItem], history: HistoryWindow?, follows: Bool) -> [TranscriptEntry] {
        if generation != history?.generation {
            page = []
            previous = []
            pinned = true
            generation = history?.generation
        }
        if follows && !self.follows {
            pinned = true
            previous = []
        }
        self.follows = follows
        if start != history?.start || textStart != history?.textStart {
            previous = pinned && follows ? [] : page
            start = history?.start
            textStart = history?.textStart
        }
        page = items.map { item in
            TranscriptEntry(item: item, id: .init(key: item.key,
                offset: history?.textKey == item.key ? history?.textStart ?? 0 : 0))
        }
        let current = Set(page.map(\.id))
        return (previous.filter { !current.contains($0.id) } + page).sorted {
            ($0.id.key, $0.id.offset) < ($1.id.key, $1.id.offset)
        }
    }

    func update() {
        guard let scroll, let content, scroll.contentSize.width > 0 else { return }
        let anchor = visibleAnchor()
        updating = true
        width = scroll.contentSize.width
        host.rootView = content(width)
        layout(anchor: anchor)
        updating = false
        if pinned && !follows {
            Task { [weak self] in
                guard let self, self.pinned, let store = self.store, !store.followsHistory,
                      let history = store.history, !history.pending,
                      history.end == history.total, history.textEnd == history.textTotal else { return }
                store.followLatest()
            }
        }
    }

    private func visibleAnchor() -> (TranscriptEntry.ID, CGFloat)? {
        guard let scroll else { return nil }
        let y = scroll.contentView.bounds.minY
        return markers.compactMap { id, view -> (TranscriptEntry.ID, CGFloat, CGFloat)? in
            let frame = view.convert(view.bounds, to: host)
            guard frame.maxY > y else { return nil }
            return (id, y - frame.minY, frame.minY)
        }.min { $0.2 < $1.2 }.map { ($0.0, $0.1) }
    }

    private func layout(anchor: (TranscriptEntry.ID, CGFloat)?) {
        guard let scroll else { return }
        let log = OSLog(subsystem: "dev.a.app", category: .pointsOfInterest)
        os_signpost(.begin, log: log, name: "Transcript layout")
        defer { os_signpost(.end, log: log, name: "Transcript layout") }
        host.layoutSubtreeIfNeeded()
        let height = max(host.fittingSize.height, scroll.contentSize.height)
        if host.frame.size != NSSize(width: width, height: height) {
            host.setFrameSize(NSSize(width: width, height: height))
            host.layoutSubtreeIfNeeded()
        }
        let maximum = max(0, height - scroll.contentSize.height)
        let y: CGFloat
        if pinned && follows {
            y = maximum
        } else if let (id, offset) = anchor, let marker = markers[id] {
            y = marker.convert(marker.bounds, to: host).minY + offset
        } else {
            y = scroll.contentView.bounds.minY
        }
        let target = NSPoint(x: 0, y: min(maximum, max(0, y)))
        if scroll.contentView.bounds.origin != target {
            scroll.contentView.scroll(to: target)
        }
        scroll.reflectScrolledClipView(scroll.contentView)
        lastOrigin = scroll.contentView.bounds.origin
        pinned = scroll.contentView.bounds.maxY >= height - 1
    }

    func wheel(_ delta: CGFloat) {
        guard let scroll, !updating else { return }
        let bounds = scroll.contentView.bounds
        if delta > 0 && bounds.minY <= 0 { store?.loadEarlier() }
        else if delta < 0 && bounds.maxY >= host.frame.height - 1 && !follows { store?.loadLater() }
    }

    func viewportChanged() {
        guard let scroll, !updating, width != scroll.contentSize.width else { return }
        update()
    }

    private func scheduleLayout() {
        guard !layoutPending else { return }
        layoutPending = true
        Task { [weak self] in
            guard let self else { return }
            self.layoutPending = false
            guard !self.updating else { return }
            if let scroll = self.scroll, self.width != scroll.contentSize.width {
                self.update()
                return
            }
            self.updating = true
            self.layout(anchor: self.visibleAnchor())
            self.updating = false
        }
    }

    @objc private func scrolled() {
        guard let scroll, !updating else { return }
        let bounds = scroll.contentView.bounds
        if bounds.origin != lastOrigin {
            pinned = bounds.maxY >= host.frame.height - 1
            lastOrigin = bounds.origin
        }
        if width != scroll.contentSize.width {
            update()
            return
        }
        if bounds.minY <= 0 && host.frame.height > bounds.height {
            store?.loadEarlier()
        } else if pinned && !follows {
            store?.loadLater()
        }
    }
}

private final class TranscriptScroller: NSScrollView {
    weak var owner: TranscriptCoordinator?

    override func layout() {
        super.layout()
        owner?.viewportChanged()
    }

    override func scrollWheel(with event: NSEvent) {
        super.scrollWheel(with: event)
        owner?.wheel(event.scrollingDeltaY)
    }
}

private final class TranscriptHost: NSHostingView<AnyView> {
    var invalidated: (() -> Void)?

    override func invalidateIntrinsicContentSize() {
        super.invalidateIntrinsicContentSize()
        invalidated?()
    }
}

private struct TranscriptMarker: NSViewRepresentable {
    let id: TranscriptEntry.ID
    weak var coordinator: TranscriptCoordinator?

    final class Registration {
        let id: TranscriptEntry.ID
        weak var owner: TranscriptCoordinator?

        init(id: TranscriptEntry.ID, owner: TranscriptCoordinator?) {
            self.id = id
            self.owner = owner
        }
    }

    func makeCoordinator() -> Registration { Registration(id: id, owner: coordinator) }

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        coordinator?.markers[id] = view
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        coordinator?.markers[id] = view
    }

    static func dismantleNSView(_ view: NSView, coordinator: Registration) {
        if coordinator.owner?.markers[coordinator.id] === view {
            coordinator.owner?.markers.removeValue(forKey: coordinator.id)
        }
    }
}
