import AppKit

/// Plain transcript rows can be measured and displayed without a SwiftUI layout tree.
public enum NativeTranscriptText: Equatable {
    case user(String)
    case text(String)
    case markdown(ShownBody)
}

struct NativeTextMeasurement {
    let width: CGFloat
    let size: NSSize
    let height: CGFloat
    let attributed: NSAttributedString?
}

final class TranscriptPlainCell: NSTableCellView {
    private let label = NSTextField(wrappingLabelWithString: "")
    private var bubbleFrame = NSRect.zero
    private var content: NativeTranscriptText?
    private var measured: NativeTextMeasurement?

    override var isFlipped: Bool { true }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        label.isSelectable = true
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        addSubview(label)
    }

    required init?(coder: NSCoder) { nil }

    func show(_ content: NativeTranscriptText, measured: NativeTextMeasurement? = nil) {
        if self.content != content { self.measured = nil }
        if let measured { self.measured = measured }
        self.content = content
        switch content {
        case .user(let text):
            if label.stringValue != text { label.stringValue = text }
            label.font = .preferredFont(forTextStyle: .body)
        case .text(let text):
            if label.stringValue != text { label.stringValue = text }
            label.font = .monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        case .markdown(let body):
            label.attributedStringValue = measured?.attributed ?? markdown(body)
            label.allowsEditingTextAttributes = true
        }
        needsLayout = true
        needsDisplay = true
    }

    func measurement(for width: CGFloat) -> NativeTextMeasurement {
        if let measured, measured.width == width { return measured }
        let available: CGFloat
        if case .user = content { available = max(1, width - 32 - 80 - 24) }
        else { available = max(1, width - 32) }
        let size = label.cell?.cellSize(forBounds: NSRect(x: 0, y: 0, width: available, height: .greatestFiniteMagnitude)) ?? .zero
        let text = NSSize(width: min(available, ceil(size.width)), height: max(1, ceil(size.height)))
        let padding: CGFloat
        if case .user = content { padding = 26 } else { padding = 10 }
        let measured = NativeTextMeasurement(width: width, size: text, height: text.height + padding, attributed: { if case .markdown = content { label.attributedStringValue } else { nil } }())
        self.measured = measured
        return measured
    }

    override func layout() {
        super.layout()
        let text = measurement(for: bounds.width).size
        if case .user = content {
            let x = bounds.width - 16 - text.width - 24
            bubbleFrame = NSRect(x: x, y: 5, width: text.width + 24, height: text.height + 16)
            label.frame = NSRect(x: x + 12, y: 13, width: text.width, height: text.height)
        } else {
            bubbleFrame = .zero
            label.frame = NSRect(x: 16, y: 5, width: max(0, bounds.width - 32), height: text.height)
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        if case .user = content {
            NSColor.controlAccentColor.withAlphaComponent(0.15).setFill()
            NSBezierPath(roundedRect: bubbleFrame, xRadius: 12, yRadius: 12).fill()
        }
    }

    private func markdown(_ body: ShownBody) -> NSAttributedString {
        let result = NSMutableAttributedString(string: "")
        let font = NSFont.preferredFont(forTextStyle: .body)
        if let runs = body.runs {
            for run in runs {
                let shown: NSFont = switch run.style {
                case .text: font
                case .bold: NSFontManager.shared.convert(font, toHaveTrait: .boldFontMask)
                case .code: .monospacedSystemFont(ofSize: font.pointSize, weight: .regular)
                }
                result.append(NSAttributedString(string: run.text, attributes: [.font: shown, .foregroundColor: NSColor.labelColor]))
            }
            return result
        }
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        let parsed = (try? AttributedString(markdown: body.text, options: options)) ?? AttributedString(body.text)
        for run in parsed.runs {
            var shown = font
            if run.inlinePresentationIntent?.contains(.code) == true {
                shown = .monospacedSystemFont(ofSize: font.pointSize, weight: .regular)
            }
            if run.inlinePresentationIntent?.contains(.stronglyEmphasized) == true {
                shown = NSFontManager.shared.convert(shown, toHaveTrait: .boldFontMask)
            }
            if run.inlinePresentationIntent?.contains(.emphasized) == true {
                shown = NSFontManager.shared.convert(shown, toHaveTrait: .italicFontMask)
            }
            var attributes: [NSAttributedString.Key: Any] = [.font: shown, .foregroundColor: NSColor.labelColor]
            if let link = run.link { attributes[.link] = link }
            if run.inlinePresentationIntent?.contains(.strikethrough) == true {
                attributes[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
            }
            result.append(NSAttributedString(string: String(parsed[run.range].characters), attributes: attributes))
        }
        return result
    }

    func clear() {
        content = nil
        measured = nil
        label.stringValue = ""
    }
}
