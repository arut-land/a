import AppKit
import SwiftUI

public struct ConversationSwitcher: View {
    private let store: ConversationStore
    private let current: String?
    @Binding private var managing: Bool
    private let open: (String) -> Void

    public init(store: ConversationStore, current: String?, managing: Binding<Bool>, open: @escaping (String) -> Void) {
        self.store = store
        self.current = current
        self._managing = managing
        self.open = open
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            ConversationList(rows: store.rows, current: current, managing: $managing, open: open, stop: store.interrupt)
                .frame(height: min(140, CGFloat(store.rows.count) * 26 + 4))
            if managing { Text("↑/↓ to select · Return to view · x to stop · Escape to compose").font(.caption).foregroundStyle(.secondary) }
            if let problem = store.problem { Text(problem).font(.caption).foregroundStyle(.red) }
        }
        .accessibilityIdentifier("conversation-switcher")
    }
}

private struct ConversationList: NSViewRepresentable {
    let rows: [ConversationInfo]
    let current: String?
    @Binding var managing: Bool
    let open: (String) -> Void
    let stop: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = ConversationScroll()
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        let table = ConversationTable()
        table.owner = context.coordinator
        table.addTableColumn(NSTableColumn(identifier: .init("conversation")))
        table.addTableColumn(NSTableColumn(identifier: .init("progress")))
        table.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        table.headerView = nil
        table.backgroundColor = .clear
        table.rowHeight = 26
        table.intercellSpacing = .zero
        table.delegate = context.coordinator
        table.dataSource = context.coordinator
        table.allowsEmptySelection = true
        table.setAccessibilityLabel("Agent conversations")
        scroll.documentView = table
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let table = scroll.documentView as! ConversationTable
        let coordinator = context.coordinator
        coordinator.open = open
        coordinator.stop = stop
        coordinator.leave = { managing = false }
        if coordinator.rows != rows || coordinator.current != current {
            let selected = coordinator.rows.indices.contains(table.selectedRow) ? coordinator.rows[table.selectedRow].id : current
            coordinator.rows = rows
            coordinator.current = current
            table.reloadData()
            if let at = rows.firstIndex(where: { $0.id == selected }) { table.selectRowIndexes(IndexSet(integer: at), byExtendingSelection: false) }
        }
        if managing && !coordinator.managing {
            if !rows.isEmpty { table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false) }
            Task { [weak table] in
                guard let table else { return }
                table.window?.makeFirstResponder(table)
            }
        }
        coordinator.managing = managing
    }

    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
        var rows: [ConversationInfo] = []
        var current: String?
        var managing = false
        var open: ((String) -> Void)?
        var stop: ((String) -> Void)?
        var leave: (() -> Void)?

        func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            let progress = tableColumn?.identifier.rawValue == "progress"
            let identifier = NSUserInterfaceItemIdentifier(progress ? "conversation-progress" : "conversation-row")
            let text = tableView.makeView(withIdentifier: identifier, owner: nil) as? NSTextField
                ?? NSTextField(labelWithString: "")
            text.identifier = identifier
            let item = rows[row]
            let marker = item.id == current ? "●" : "○"
            let title = item.depth == 0 ? "main" : "\(item.agent)  \(item.title)"
            text.stringValue = progress ? item.detail : "\(marker) \(String(repeating: "  ", count: Int(item.depth)))\(title)"
            text.alignment = progress ? .right : .left
            text.font = .preferredFont(forTextStyle: .callout)
            text.lineBreakMode = .byTruncatingTail
            text.setAccessibilityLabel(text.stringValue)
            return text
        }

        func activate(_ at: Int) {
            guard rows.indices.contains(at) else { return }
            open?(rows[at].id)
        }
    }
}

private final class ConversationTable: NSTableView {
    weak var owner: ConversationList.Coordinator?

    override func keyDown(with event: NSEvent) {
        guard let owner else { return }
        switch event.keyCode {
        case 36, 76: owner.activate(selectedRow)
        case 53: owner.leave?()
        case 126 where selectedRow <= 0: owner.leave?()
        default:
            if event.charactersIgnoringModifiers == "x", owner.rows.indices.contains(selectedRow) {
                let row = owner.rows[selectedRow]
                if row.parent != nil && row.running { owner.stop?(row.id) }
            } else { super.keyDown(with: event) }
        }
    }

    override func mouseDown(with event: NSEvent) {
        super.mouseDown(with: event)
        owner?.activate(clickedRow)
    }
}

private final class ConversationScroll: NSScrollView {
    override func layout() {
        super.layout()
        guard let table = documentView as? NSTableView else { return }
        table.setFrameSize(NSSize(width: contentSize.width, height: table.frame.height))
        let progress = min(260, contentSize.width * 0.4)
        table.tableColumns[0].width = max(0, contentSize.width - progress)
        table.tableColumns[1].width = progress
    }
}
