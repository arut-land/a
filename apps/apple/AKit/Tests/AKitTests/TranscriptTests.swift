import AppKit
import SwiftUI
import Observation
import Testing

@testable import AKit

@Suite(.serialized)
struct NativeTranscriptTests {
    @Test
    func streaming_growth_remeasures_only_the_reply_and_preserves_viewport_pinning() async throws {
        let vector = try #require(Vector.all.first { $0.name == "retry replaces streamed text at every length" })
        let reducer = Reducer()
        let store = SessionStore(source: reducer)
        for message in vector.messages {
            if let update = try reducer.fold(message: message) { store.apply(update) }
            if store.streaming != nil && store.items.contains(where: { $0.kind == .reply }) { break }
        }
        let stream = try #require(store.streaming)
        let items = store.items
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 300),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let host = NSHostingView(rootView: TranscriptView(store: store) { item in
            Text(item.kind == .reply ? store.streaming?.text ?? "" : item.presentation.body?.text ?? "")
                .font(.body.monospaced()).fixedSize(horizontal: false, vertical: true)
        })
        window.contentView = host
        window.orderFront(nil)
        try await Task.sleep(for: .milliseconds(100))
        let scroll = try #require(findScrollView(host))
        let table = try #require(scroll.documentView as? NSTableView)
        let row = try #require(items.firstIndex { $0.key == stream.call })
        let before = table.rect(ofRow: row).height
        func appendLines() {
            store.apply(ChangeBatch(history: nil, revision: store.rev + 1, retainedBytes: 0, seq: store.seq,
                changes: [.delta(call: stream.call, text: String(repeating: "streamed line\n", count: 30))],
                turn: store.turn, streaming: StreamId(call: stream.call, attempt: stream.attempt), title: store.title))
        }
        appendLines()
        try await eventually("streamed reply grows without an item change") { table.rect(ofRow: row).height > before + 300 }
        #expect(store.items == items)
        #expect(abs(scroll.contentView.bounds.maxY - table.frame.height) <= 1)
        scroll.contentView.scroll(to: NSPoint(x: 0, y: 50))
        scroll.reflectScrolledClipView(scroll.contentView)
        try await Task.sleep(for: .milliseconds(100))
        let origin = scroll.contentView.bounds.origin
        let height = table.rect(ofRow: row).height
        appendLines()
        try await eventually("scrolled-up reply continues growing") { table.rect(ofRow: row).height > height + 300 }
        #expect(scroll.contentView.bounds.origin == origin)
        #expect(store.items == items)
    }

    @Test(.enabled(if: live != nil))
    func external_updates_preserve_the_native_viewport_when_scrolled_up() async throws {
        let client = try Client(helper: live!)
        let session = Session(client: client)
        let store = SessionStore(session: session)
        let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-transcript-\(UUID())")
        try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: workspace) }
        store.open(.new(workspace: workspace.path))
        try await eventually("native session attached") { store.link == .live && store.history?.pending == false }
        for turn in 0..<40 {
            let marker = "turn \(turn)"
            await store.send(String(repeating: "history line\n", count: 160) + marker)
            try await eventually("native transcript populated") {
                !store.turn.running && store.history?.pending == false && store.items.contains {
                    $0.kind == .assistant && $0.presentation.body?.text.hasSuffix(marker) == true
                }
            }
        }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 800, height: 500),
                              styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let frames = TranscriptFrames()
        let host = NSHostingView(rootView: TranscriptView(store: store) { item in
            Text(item.presentation.body?.text ?? item.presentation.title)
                .font(.body.monospaced()).fixedSize(horizontal: false, vertical: true)
                .background {
                    GeometryReader { geometry in
                        Color.clear.onChange(of: geometry.frame(in: .global), initial: true) { _, frame in
                            frames.items[item.key] = frame
                        }.onDisappear { frames.items.removeValue(forKey: item.key) }
                    }
                }
        })
        window.contentView = host
        window.orderFront(nil)
        try await eventually("native scroll view installed") { findScrollView(host) != nil }
        let scroll = try #require(findScrollView(host))
        let document = try #require(scroll.documentView)
        try await eventually("native history laid out at its bottom") {
            document.frame.height > scroll.contentView.bounds.height * 3 &&
                abs(scroll.contentView.bounds.maxY - document.frame.height) <= 1
        }
        let table = try #require(document as? NSTableView)
        var realized = 0
        table.enumerateAvailableRowViews { _, _ in realized += 1 }
        #expect(realized < 8 && table.numberOfRows > 20,
                "long history must retain only visible row views, not a laid-out page")
        let target = max(100, scroll.contentView.bounds.minY - 300)
        scroll.contentView.scroll(to: NSPoint(x: 0, y: target))
        scroll.reflectScrolledClipView(scroll.contentView)
        try await Task.sleep(for: .milliseconds(150))
        let before = scroll.contentView.bounds.origin
        #expect(abs(before.y - target) <= 1, "native scrolling must leave the viewport at the requested position")
        let writer = Session(client: client)
        try writer.open(target: .id(id: try #require(store.sessionId)))
        try await eventually("external writer attached") { writer.snapshot().link == .live }
        let seq = store.seq
        try writer.send(text: "external update while reading earlier output")
        try await eventually("external update rendered") {
            store.seq > seq && !store.turn.running && store.history?.pending == false
        }
        try await Task.sleep(for: .milliseconds(200))
        #expect(abs(scroll.contentView.bounds.origin.y - before.y) <= 1,
                "an external update moved a scrolled-up native viewport")
        #expect(store.followsHistory, "the test must stay in the live requested range")
        scroll.contentView.scroll(to: NSPoint(x: 0, y: 100))
        scroll.reflectScrolledClipView(scroll.contentView)
        try await Task.sleep(for: .milliseconds(150))
        let visible = frames.items.filter { $0.value.intersects(host.bounds) }
        let visibleAnchor = try #require(visible.min { $0.value.minY < $1.value.minY })
        let oldRange = try #require(store.history)
        let revision = store.rev
        store.loadEarlier()
        try await eventually("earlier native page installed") { store.rev > revision && store.history?.pending == false }
        try await Task.sleep(for: .milliseconds(200))
        #expect(store.history?.start != oldRange.start || store.history?.textStart != oldRange.textStart)
        let retainedFrame = try #require(frames.items[visibleAnchor.key], "the visible item must survive its adjacent page delivery")
        #expect(abs(retainedFrame.minY - visibleAnchor.value.minY) <= 1,
                "prepending a demanded page must preserve the visible anchor's screen position")
        scroll.contentView.scroll(to: NSPoint(x: 0, y: 100))
        scroll.reflectScrolledClipView(scroll.contentView)
        try await Task.sleep(for: .milliseconds(100))
        let olderRevision = store.rev
        store.loadEarlier()
        try await eventually("another earlier native page") { store.rev > olderRevision && store.history?.pending == false }
        try await Task.sleep(for: .milliseconds(100))
        let firstLater = store.rev
        scroll.contentView.scroll(to: NSPoint(x: 0, y: document.frame.height - scroll.contentView.bounds.height))
        scroll.reflectScrolledClipView(scroll.contentView)
        try await eventually("cached later page") { store.rev > firstLater && store.history?.pending == false }
        try await Task.sleep(for: .milliseconds(100))
        let laterAnchor = try #require(frames.items.filter { $0.value.intersects(host.bounds) }.min { $0.value.minY < $1.value.minY })
        let nextLater = store.rev
        store.loadLater()
        try await eventually("next later page") { store.rev > nextLater && store.history?.pending == false }
        try await Task.sleep(for: .milliseconds(100))
        let laterFrame = try #require(frames.items[laterAnchor.key])
        #expect(abs(laterFrame.minY - laterAnchor.value.minY) <= 1,
                "loading a later page must preserve the visible item rather than jump to its end")
        store.followLatest()
        try await eventually("native transcript follows latest again") { store.followsHistory && store.history?.pending == false }
        try await Task.sleep(for: .milliseconds(100))
        let latest = store.seq
        try writer.send(text: "external update at the bottom")
        try await eventually("pinned external output rendered") { store.seq > latest && !store.turn.running && store.history?.pending == false }
        try await Task.sleep(for: .milliseconds(100))
        #expect(abs(scroll.contentView.bounds.maxY - document.frame.height) <= 1,
                "output must follow when the viewport is actually pinned to the bottom")
        try writer.detach()
        await store.detach()
    }
    @Test(.enabled(if: live != nil))
    func plain_transcript_rows_use_selectable_native_cells_and_preserve_the_viewport() async throws {
        let client = try Client(helper: live!)
        let store = SessionStore(session: Session(client: client))
        let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-native-text-\(UUID())")
        try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: workspace) }
        store.open(.new(workspace: workspace.path))
        try await eventually("native text session") { store.link == .live }
        for turn in 0..<16 {
            let marker = "native turn \(turn)"
            await store.send(String(repeating: "歴史 🦀 history line\n", count: 160) + marker)
            try await eventually("native text history") {
                store.history?.pending == false && store.items.contains { $0.kind == .assistant && $0.presentation.body?.text.hasSuffix(marker) == true }
            }
        }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 800, height: 500), styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let host = NSHostingView(rootView: TranscriptView(store: store, text: { item in
            item.kind == .user ? .user(item.presentation.body?.text ?? "") : .text(item.presentation.body?.text ?? "")
        }) { item in Text(item.presentation.title) })
        window.contentView = host
        window.orderFront(nil)
        try await eventually("native text table installed") { findScrollView(host)?.documentView is NSTableView }
        let scroll = try #require(findScrollView(host))
        let table = try #require(scroll.documentView as? NSTableView)
        try await eventually("native text scrolled to tail") {
            table.frame.height > 1500 && abs(scroll.contentView.bounds.maxY - table.frame.height) <= 1
        }
        let labels = nativeLabels(table)
        #expect(!labels.isEmpty && labels.allSatisfy { $0.isSelectable })
        #expect(labels.contains { $0.stringValue.contains("歴史 🦀") })
        scroll.contentView.scroll(to: NSPoint(x: 0, y: scroll.contentView.bounds.minY - 300))
        scroll.reflectScrolledClipView(scroll.contentView)
        try await Task.sleep(for: .milliseconds(100))
        let before = scroll.contentView.bounds.origin
        let writer = Session(client: client)
        try writer.open(target: .id(id: try #require(store.sessionId)))
        try await eventually("native text external writer") { writer.snapshot().link == .live }
        let seq = store.seq
        try writer.send(text: "native external update")
        try await eventually("native external update displayed") { store.seq > seq && store.history?.pending == false && !store.turn.running }
        try await Task.sleep(for: .milliseconds(100))
        #expect(abs(scroll.contentView.bounds.origin.y - before.y) <= 1)
        try writer.detach()
        await store.detach()
    }

    @Test
    func native_markdown_keeps_inline_styles_links_and_fragment_continuations() throws {
        let cell = TranscriptPlainCell()
        cell.show(.markdown(ShownBody(kind: .markdown, text: "**bold** and `code` [link](https://example.invalid)", runs: nil)))
        let attributed = try #require(cell.measurement(for: 500).attributed)
        #expect(attributed.string == "bold and code link")
        let bold = try #require(attributed.attribute(.font, at: 0, effectiveRange: nil) as? NSFont)
        #expect(NSFontManager.shared.traits(of: bold).contains(.boldFontMask))
        let code = try #require(attributed.attribute(.font, at: 9, effectiveRange: nil) as? NSFont)
        #expect(code.isFixedPitch)
        #expect(attributed.attribute(.link, at: 14, effectiveRange: nil) as? URL == URL(string: "https://example.invalid"))
        cell.show(.markdown(ShownBody(kind: .markdown, text: "continued** plain", runs: [MarkdownRun(text: "continued", style: .bold), MarkdownRun(text: " plain", style: .text)])))
        let fragment = try #require(cell.measurement(for: 500).attributed)
        #expect(fragment.string == "continued plain")
        let continued = try #require(fragment.attribute(.font, at: 0, effectiveRange: nil) as? NSFont)
        #expect(NSFontManager.shared.traits(of: continued).contains(.boldFontMask))
    }

    @Test(.enabled(if: live != nil))
    func native_agent_list_uses_arrows_return_and_escape() async throws {
        let client = try Client(helper: live!)
        let parent = SessionStore(session: Session(client: client))
        let workspace = FileManager.default.temporaryDirectory.appending(path: "akit-picker-\(UUID())")
        try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: workspace) }
        parent.open(.new(workspace: workspace.path))
        try await eventually("picker parent attached") { parent.link == .live }
        let id = try #require(parent.sessionId)
        let store = try ConversationStore(client: client, origin: id)
        store.start()
        defer { store.stop() }
        await parent.send("spawn picker child")
        try await eventually("picker family ready") { store.rows.count == 2 && store.rows.allSatisfy { !$0.running && $0.status != "connecting" } }
        await parent.send("spawn second picker child")
        try await eventually("second picker child ready") { store.rows.count == 3 && store.rows.allSatisfy { !$0.running && $0.status != "connecting" } }
        let first = store.rows[1]
        let child = store.rows[2]
        let selection = ConversationSelection()
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 800, height: 200), styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let host = NSHostingView(rootView: ConversationSwitcher(store: store, current: id,
            managing: Binding(get: { selection.managing }, set: { selection.managing = $0 }),
            open: { selection.opened = $0 }))
        window.contentView = host
        window.makeKeyAndOrderFront(nil)
        try await Task.sleep(for: .milliseconds(100))
        selection.managing = true
        try await Task.sleep(for: .milliseconds(150))
        func key(_ code: UInt16, _ characters: String) {
            window.sendEvent(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, characters: characters, charactersIgnoringModifiers: characters,
                isARepeat: false, keyCode: code)!)
        }
        key(125, "\u{F701}")
        key(125, "\u{F701}")
        let branch = SessionStore(session: Session(client: client))
        branch.open(.id(id: first.id))
        try await eventually("first child attached for nested spawn") { branch.link == .live }
        await branch.send("spawn nested picker child")
        try await eventually("grandchild inserted before the selected child") { store.rows.count == 4 }
        try await Task.sleep(for: .milliseconds(100))
        let reordered = try #require(findScrollView(host)?.documentView as? NSTableView)
        #expect(reordered.selectedRow == 3)
        key(36, "\r")
        try await eventually("Return activates the selected child") { selection.opened == child.id }
        host.rootView = ConversationSwitcher(store: store, current: child.id,
            managing: Binding(get: { selection.managing }, set: { selection.managing = $0 }),
            open: { selection.opened = $0 })
        try await Task.sleep(for: .milliseconds(100))
        let table = try #require(findScrollView(host)?.documentView as? NSTableView)
        let marker = try #require(table.view(atColumn: 0, row: 3, makeIfNecessary: true) as? NSTextField)
        #expect(marker.stringValue.hasPrefix("●"))
        key(53, "\u{1b}")
        try await eventually("Escape returns to the composer") { !selection.managing }
        await branch.detach()
        await parent.detach()
    }

}

private func findScrollView(_ view: NSView) -> NSScrollView? {
    if let scroll = view as? NSScrollView { return scroll }
    return view.subviews.lazy.compactMap { findScrollView($0) }.first
}

private final class TranscriptFrames {
    var items: [UInt64: CGRect] = [:]
}

@Observable
private final class ConversationSelection {
    var managing = false
    var opened: String?
}

private func nativeLabels(_ view: NSView) -> [NSTextField] {
    if let label = view as? NSTextField { return [label] }
    return view.subviews.flatMap(nativeLabels)
}
