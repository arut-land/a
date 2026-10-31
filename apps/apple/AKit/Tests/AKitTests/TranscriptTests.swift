import AppKit
import SwiftUI
import Testing

@testable import AKit

@Suite(.serialized)
struct NativeTranscriptTests {
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
}

private func findScrollView(_ view: NSView) -> NSScrollView? {
    if let scroll = view as? NSScrollView { return scroll }
    return view.subviews.lazy.compactMap { findScrollView($0) }.first
}

private final class TranscriptFrames {
    var items: [UInt64: CGRect] = [:]
}
