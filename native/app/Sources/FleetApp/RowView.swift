// The board's cells, in AppKit (Ruling P3-R6), reused by the table: one session as one quiet monospace line, the
// columns of `public/fleet.html` `row()` (lines 1944-1975): the state as a dot and a word, the title, the machine, the
// account, its age, its effort and its context; the reason on hover. Then the group header with its tally, the
// "N older" line, and a note line. The colours are fleet.html's (lines 35-43), light and dark.

import AppKit
import FleetKit

@MainActor enum Palette {
    private static func dynamic(_ light: UInt32, _ dark: UInt32) -> NSColor {
        NSColor(name: nil) { a in
            let v = a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(
                srgbRed: CGFloat(v >> 16 & 0xff) / 255, green: CGFloat(v >> 8 & 0xff) / 255,
                blue: CGFloat(v & 0xff) / 255, alpha: 1)
        }
    }

    static let blue = dynamic(0x0a72a3, 0x3fb6ee)
    static let orange = dynamic(0xa5560f, 0xef9a4c)
    static let red = dynamic(0xbf3f33, 0xec7468)
    static let green = dynamic(0x2b7d52, 0x5fbd8b)
    static let grey = dynamic(0x5f666b, 0x9ba2a7)
    static let quiet = dynamic(0x858c91, 0x6c7277)
    static let rule = dynamic(0xe2e5e7, 0x2c3033)

    static func of(_ s: SessionState) -> NSColor {
        switch s {
        case .working: blue
        case .needsYou: orange
        case .capped: red
        case .waiting: grey
        case .done, .ended, .unreachable: quiet
        }
    }

    /// fleet.html `level`: 90 % and over is red, 70 % and over orange.
    static func level(_ pct: Double) -> NSColor? { pct >= 90 ? red : pct >= 70 ? orange : nil }
}

@MainActor enum Fonts {
    static let row = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
    static let small = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
    static let word = NSFont.monospacedSystemFont(ofSize: 11, weight: .medium)
}

@MainActor enum Words {
    /// fleet.html `ago`: now, 5m, 3h, 2d.
    static func ago(_ ms: Int64?, now: Date = Date()) -> String {
        guard let ms else { return "" }
        let s = max(0, now.timeIntervalSince1970 - Double(ms) / 1000)
        return s < 60 ? "now" : s < 3600 ? "\(Int((s / 60).rounded()))m"
            : s < 86400 ? "\(Int((s / 3600).rounded()))h" : "\(Int((s / 86400).rounded()))d"
    }

    /// fleet.html `kTok`: 142k, 1.2M.
    static func tokens(_ n: UInt64?) -> String {
        guard let n else { return "" }
        return n >= 1_000_000 ? "\((Double(n) / 1e5).rounded() / 10)M" : "\(Int((Double(n) / 1000).rounded()))k"
    }

    /// fleet.html `until`: 12m, 3h 5m, 2d 4h.
    static func until(_ ms: Int64, now: Date = Date()) -> String {
        let s = Double(ms) / 1000 - now.timeIntervalSince1970
        if s <= 0 { return "now" }
        if s < 3600 { return "\(Int((s / 60).rounded()))m" }
        if s < 86400 { return "\(Int(s / 3600))h \(Int((s.truncatingRemainder(dividingBy: 3600) / 60).rounded()))m" }
        return "\(Int(s / 86400))d \(Int((s.truncatingRemainder(dividingBy: 86400) / 3600).rounded()))h"
    }

    static func title(_ s: Session) -> String { s.title ?? s.name ?? s.jobId ?? "\(s.engine)" }

    static func time(_ d: Date) -> String {
        let f = DateFormatter()
        f.dateStyle = Calendar.current.isDateInToday(d) ? .none : .medium
        f.timeStyle = .short
        return f.string(from: d)
    }
}

@MainActor func label(_ font: NSFont, _ colour: NSColor = .secondaryLabelColor, align: NSTextAlignment = .left) -> NSTextField {
    let l = NSTextField(labelWithString: "")
    l.font = font
    l.textColor = colour
    l.alignment = align
    l.lineBreakMode = .byTruncatingTail
    l.usesSingleLineMode = true
    l.cell?.truncatesLastVisibleLine = true
    return l
}

/// Draws a line of runs (text, a thin meter line, a dot): the toolbar's meters and a header's tally.
final class RunsView: NSView {
    enum Run {
        case text(String, NSColor, NSFont)
        /// A 2 pt line `width` wide, filled to `fraction` (fleet.html `.line`).
        case bar(Double, NSColor, CGFloat)
        case dot(NSColor, ring: Bool)
        case gap(CGFloat)
    }

    var runs: [Run] = [] {
        didSet {
            invalidateIntrinsicContentSize()
            needsDisplay = true
        }
    }

    override var isFlipped: Bool { true }

    private static func size(_ s: String, _ f: NSFont) -> NSSize {
        (s as NSString).size(withAttributes: [.font: f])
    }

    private func width(_ r: Run) -> CGFloat {
        switch r {
        case let .text(s, _, f): ceil(Self.size(s, f).width)
        case let .bar(_, _, w): w
        case .dot: 6
        case let .gap(w): w
        }
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: runs.reduce(0) { $0 + width($1) }, height: 18)
    }

    override func draw(_ dirtyRect: NSRect) {
        var x: CGFloat = 0
        let mid = bounds.midY
        for r in runs {
            switch r {
            case let .text(s, c, f):
                let sz = Self.size(s, f)
                (s as NSString).draw(
                    at: NSPoint(x: x, y: mid - sz.height / 2), withAttributes: [.font: f, .foregroundColor: c])
            case let .bar(fraction, c, w):
                Palette.rule.setFill()
                NSRect(x: x, y: mid - 1, width: w, height: 2).fill()
                c.setFill()
                NSRect(x: x, y: mid - 1, width: w * min(1, max(0, fraction)), height: 2).fill()
            case let .dot(c, ring):
                let p = NSBezierPath(ovalIn: NSRect(x: x, y: mid - 3, width: 6, height: 6).insetBy(dx: 0.5, dy: 0.5))
                c.set()
                if ring { p.lineWidth = 1.5; p.stroke() } else { p.fill() }
            case .gap: break
            }
            x += width(r)
        }
    }

    override func viewDidChangeEffectiveAppearance() { needsDisplay = true }
}

/// fleet.html `.dot`: filled in the state's colour; waiting is a ring; done and ended are quiet and faded.
final class DotView: NSView {
    var state: SessionState = .waiting { didSet { if state != oldValue { needsDisplay = true } } }

    override func draw(_ dirtyRect: NSRect) {
        let r = bounds.insetBy(dx: 0.75, dy: 0.75)
        let p = NSBezierPath(ovalIn: r)
        if state == .waiting {
            Palette.quiet.setStroke()
            p.lineWidth = 1.5
            p.stroke()
        } else {
            Palette.of(state).withAlphaComponent(state.order >= 4 ? 0.7 : 1).setFill()
            p.fill()
        }
    }

    override func viewDidChangeEffectiveAppearance() { needsDisplay = true }
}

/// One session row, drawn rather than built from text fields: the table makes and lays out about thirty of these for a
/// window, and one view that draws eight strings is far cheaper than eight NSTextFields (Ruling P3-R6's launch budget).
final class RowCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("row")
    private var session: Session?
    private var ageText = ""

    private static func style(_ align: NSTextAlignment) -> NSParagraphStyle {
        let p = NSMutableParagraphStyle()
        p.lineBreakMode = .byTruncatingTail
        p.alignment = align
        return p
    }

    private static let leftStyle = style(.left)
    private static let rightStyle = style(.right)

    init() {
        super.init(frame: .zero)
        identifier = Self.id
    }

    required init?(coder: NSCoder) { fatalError() }

    func show(_ s: Session, cached: Bool) {
        session = s
        ageText = Words.ago(s.lastAt ?? s.startedAt)
        alphaValue = cached ? 0.6 : 1
        toolTip = s.reason
        setAccessibilityLabel("\(Words.title(s)), \(s.state.word), \(s.reason)")
        needsDisplay = true
    }

    /// The 30 s tick: the age is the only thing the clock moves.
    func tick(now: Date) {
        guard let s = session else { return }
        let a = Words.ago(s.lastAt ?? s.startedAt, now: now)
        if a != ageText {
            ageText = a
            needsDisplay = true
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let s = session else { return }
        let on = backgroundStyle == .emphasized ? NSColor.alternateSelectedControlTextColor : nil
        let h = bounds.height, lh: CGFloat = 16, y = (h - lh) / 2
        func text(_ t: String, _ x: CGFloat, _ w: CGFloat, _ f: NSFont, _ c: NSColor, _ p: NSParagraphStyle = RowCell.leftStyle) {
            guard !t.isEmpty, w > 0 else { return }
            (t as NSString).draw(
                with: NSRect(x: x, y: y, width: w, height: lh), options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine],
                attributes: [.font: f, .foregroundColor: on ?? c, .paragraphStyle: p])
        }
        // fleet.html `.dot`: filled in the state's colour; waiting is a ring; done and ended are quiet and faded.
        let dot = NSBezierPath(ovalIn: NSRect(x: 6, y: (h - 7) / 2, width: 7, height: 7).insetBy(dx: 0.75, dy: 0.75))
        if s.state == .waiting {
            (on ?? Palette.quiet).setStroke()
            dot.lineWidth = 1.5
            dot.stroke()
        } else {
            (on ?? Palette.of(s.state)).withAlphaComponent(s.state.order >= 4 ? 0.7 : 1).setFill()
            dot.fill()
        }
        text(s.state.word, 19, 74, s.state.order <= 2 ? Fonts.word : Fonts.small, Palette.of(s.state))
        // From the right: context, effort, age, account, machine; the title takes what is left.
        let grey = NSColor.secondaryLabelColor
        var x = bounds.width - 6
        func col(_ t: String, _ w: CGFloat, _ p: NSParagraphStyle = RowCell.leftStyle) {
            x -= w
            text(t, x, w, Fonts.small, grey, p)
            x -= 10
        }
        col(Words.tokens(s.contextTokens), 40, Self.rightStyle)
        col(s.effort ?? "", 54)
        col(ageText, 32, Self.rightStyle)
        col(s.account ?? "", 64)
        col(s.key.machine, 64)
        text(Words.title(s), 101, x - 101, Fonts.row, s.live ? .labelColor : grey)
    }

    /// On the selection's accent fill every mark is drawn in the selected-text colour, so it stays legible.
    override var backgroundStyle: NSView.BackgroundStyle { didSet { if backgroundStyle != oldValue { needsDisplay = true } } }

    override var isFlipped: Bool { true }
    override func viewDidChangeEffectiveAppearance() { needsDisplay = true }
}

final class HeaderCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("header")
    private let name = label(Fonts.small)
    private let tally = RunsView()

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        addSubview(name)
        addSubview(tally)
    }

    required init?(coder: NSCoder) { fatalError() }

    /// The name sits at the foot of its row, with the space above it, as a section's heading does.
    override var isFlipped: Bool { true }

    func show(name n: String, tally t: [BoardRow.Tally]) {
        name.stringValue = n
        tally.runs = t.enumerated().flatMap { i, x -> [RunsView.Run] in
            (i > 0 ? [.gap(12)] : []) + [.text("\(x.count) \(x.state.word)", Palette.of(x.state), Fonts.small)]
        }
    }

    override func layout() {
        super.layout()
        let w = tally.intrinsicContentSize.width, y = bounds.height - 22
        tally.frame = NSRect(x: bounds.width - 6 - w, y: y, width: w, height: 18)
        name.frame = NSRect(x: 6, y: y + 1, width: max(0, bounds.width - 24 - w), height: 16)
    }
}

/// A line of quiet text: the "N older" fold, or a note above the groups.
final class LineCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("line")
    let text = label(Fonts.small)

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        addSubview(text)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        text.frame = NSRect(x: 6, y: (bounds.height - 16) / 2, width: max(0, bounds.width - 12), height: 16)
    }
}
