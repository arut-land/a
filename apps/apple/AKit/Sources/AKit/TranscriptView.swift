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

private struct TranscriptEntry: Equatable {
    struct ID: Hashable {
        let key: UInt64
        let offset: UInt64
    }
    let item: DisplayItem
    let id: ID
}

private enum TranscriptRow: Equatable {
    case earlier(Bool)
    case item(TranscriptEntry)
    case later(Bool)
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
        coordinator.content = { entry, width in
            AnyView(Group {
                switch entry {
                case .earlier(let pending):
                    Button("Earlier messages") { store.loadEarlier() }.disabled(pending)
                case .item(let entry):
                    row(entry.item)
                case .later(let pending):
                    HStack {
                        Button("Later messages") { store.loadLater() }.disabled(pending)
                        Button("Latest messages") { store.followLatest() }
                    }
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 16).padding(.vertical, 5)
                .frame(width: width, alignment: .topLeading))
        }
        var rows: [TranscriptRow] = []
        if let history, history.start > 0 || history.textStart > 0 {
            rows.append(.earlier(history.pending))
        }
        rows += entries.map(TranscriptRow.item)
        if !follows { rows.append(.later(history?.pending == true)) }
        coordinator.update(rows)
    }
}

private final class TranscriptCoordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    private var page: [TranscriptEntry] = []
    private var previous: [TranscriptEntry] = []
    private var rows: [TranscriptRow] = []
    private var heights: [TranscriptEntry.ID: (DisplayItem, CGFloat)] = [:]
    private var generation: UInt32?
    private var start: UInt64?
    private var textStart: UInt64?
    private var follows = true
    private var pinned = true
    private var updating = false
    private var lastOrigin: NSPoint = .zero
    private var width: CGFloat = 0
    private weak var scroll: NSScrollView?
    private let table = NSTableView()
    private let measurement = NSHostingView(rootView: AnyView(EmptyView()))
    var store: SessionStore?
    var content: ((TranscriptRow, CGFloat) -> AnyView)?

    func connect(_ scroll: NSScrollView) {
        self.scroll = scroll
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("transcript"))
        table.addTableColumn(column)
        table.headerView = nil
        table.backgroundColor = .clear
        table.selectionHighlightStyle = .none
        table.intercellSpacing = .zero
        table.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        table.dataSource = self
        table.delegate = self
        scroll.documentView = table
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

    func update(_ next: [TranscriptRow]) {
        guard let scroll, scroll.contentSize.width > 0 else { rows = next; return }
        let anchor = visibleAnchor()
        updating = true
        let resized = width != scroll.contentSize.width
        width = scroll.contentSize.width
        let keep = Set(next.compactMap { if case .item(let entry) = $0 { entry.id } else { nil } })
        heights = resized ? [:] : heights.filter { keep.contains($0.key) }
        let changed = IndexSet(next.indices.filter { $0 >= rows.count || rows[$0] != next[$0] })
        let replaced = rows.count != next.count || zip(rows, next).contains { old, new in
            if case .item(let a) = old, case .item(let b) = new { return a.id != b.id }
            return false
        }
        rows = next
        table.tableColumns[0].width = width
        table.setFrameSize(NSSize(width: width, height: table.frame.height))
        let log = OSLog(subsystem: "dev.a.app", category: .pointsOfInterest)
        os_signpost(.begin, log: log, name: "Transcript layout")
        if replaced || resized { table.reloadData() }
        else if !changed.isEmpty {
            table.noteHeightOfRows(withIndexesChanged: changed)
            table.reloadData(forRowIndexes: changed, columnIndexes: IndexSet(integer: 0))
        }
        table.layoutSubtreeIfNeeded()
        // A single measuring host replaces full-page retained SwiftUI layout trees.
        measurement.rootView = AnyView(EmptyView())
        measurement.layoutSubtreeIfNeeded()
        let maximum = max(0, table.frame.height - scroll.contentSize.height)
        let y: CGFloat
        if pinned && follows { y = maximum }
        else if let (id, offset) = anchor,
                let index = rows.firstIndex(where: { if case .item(let entry) = $0 { entry.id == id } else { false } }) {
            y = table.rect(ofRow: index).minY + offset
        } else { y = scroll.contentView.bounds.minY }
        let target = NSPoint(x: 0, y: min(maximum, max(0, y)))
        if scroll.contentView.bounds.origin != target { scroll.contentView.scroll(to: target) }
        scroll.reflectScrolledClipView(scroll.contentView)
        lastOrigin = scroll.contentView.bounds.origin
        pinned = scroll.contentView.bounds.maxY >= table.frame.height - 1
        os_signpost(.end, log: log, name: "Transcript layout")
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
        let range = table.rows(in: scroll.documentVisibleRect)
        guard range.location != NSNotFound else { return nil }
        return (range.location..<min(rows.count, range.location + range.length)).compactMap { index in
            if case .item(let entry) = rows[index] { (entry.id, y - table.rect(ofRow: index).minY) }
            else { nil }
        }.first
    }

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
        let entry = rows[row]
        if case .item(let item) = entry, let (old, height) = heights[item.id], old == item.item { return height }
        measurement.rootView = content?(entry, width) ?? AnyView(EmptyView())
        measurement.setFrameSize(NSSize(width: width, height: 1))
        let height = max(1, ceil(measurement.fittingSize.height))
        if case .item(let item) = entry { heights[item.id] = (item.item, height) }
        return height
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let id = NSUserInterfaceItemIdentifier("transcript-row")
        let host = table.makeView(withIdentifier: id, owner: nil) as? NSHostingView<AnyView>
            ?? NSHostingView(rootView: AnyView(EmptyView()))
        host.identifier = id
        host.rootView = content?(rows[row], width) ?? AnyView(EmptyView())
        return host
    }

    func tableView(_ tableView: NSTableView, didRemove rowView: NSTableRowView, forRow row: Int) {
        for case let host as NSHostingView<AnyView> in rowView.subviews {
            host.rootView = AnyView(EmptyView())
            host.layoutSubtreeIfNeeded()
        }
    }

    func wheel(_ delta: CGFloat) {
        guard let scroll, !updating else { return }
        let bounds = scroll.contentView.bounds
        if delta > 0 && bounds.minY <= 0 { store?.loadEarlier() }
        else if delta < 0 && bounds.maxY >= table.frame.height - 1 && !follows { store?.loadLater() }
    }

    func viewportChanged() {
        guard let scroll, !updating, width != scroll.contentSize.width else { return }
        update(rows)
    }

    @objc private func scrolled() {
        guard let scroll, !updating else { return }
        let bounds = scroll.contentView.bounds
        if bounds.origin != lastOrigin {
            pinned = bounds.maxY >= table.frame.height - 1
            lastOrigin = bounds.origin
        }
        if width != scroll.contentSize.width { update(rows); return }
        if bounds.minY <= 0 && table.frame.height > bounds.height { store?.loadEarlier() }
        else if pinned && !follows { store?.loadLater() }
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
