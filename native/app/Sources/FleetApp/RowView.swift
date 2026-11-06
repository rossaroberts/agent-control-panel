// The rail's cells, in AppKit (Ruling P3-R6), reused by the table: one session as a compact two-line row (Ruling
// P3-R8, like fleet.html's rail): the state dot and the title; then the state word, its age and its account; the
// reason on hover. Then the group header with its tally, the "N older" line, and a note line. The colours are
// fleet.html's (lines 35-43), light and dark.

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

    private static let formatter: DateFormatter = {
        let f = DateFormatter()
        f.timeStyle = .short
        return f
    }()

    static func time(_ d: Date) -> String {
        formatter.dateStyle = Calendar.current.isDateInToday(d) ? .none : .medium
        return formatter.string(from: d)
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
    override init(frame: NSRect) {
        super.init(frame: frame)
        clipsToBounds = true
    }

    required init?(coder: NSCoder) { fatalError() }

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

    /// The runs as whole pieces (an account's meters, one vital): as many as fit in `maxWidth`, from the left; the
    /// ones that do not fit are left out whole, never drawn over their neighbours.
    /// When not even the first piece fits, the first run of each (an account's name) stands in, as many as fit; the
    /// view is never empty, so the toolbar never pushes it into its overflow menu.
    func fit(_ pieces: [[Run]], maxWidth: CGFloat, gap: CGFloat) {
        func take(_ ps: [[Run]]) -> [Run] {
            var out: [Run] = [], used: CGFloat = 0
            for (i, p) in ps.enumerated() {
                let w = p.reduce(0) { $0 + width($1) } + (i > 0 ? gap : 0)
                if used + w > maxWidth { break }
                if i > 0 { out.append(.gap(gap)) }
                out += p
                used += w
            }
            return out
        }
        let full = take(pieces)
        runs = full.isEmpty ? take(pieces.compactMap { $0.first.map { [$0] } }) : full
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

    /// Never zero wide: a toolbar item whose view measures zero is logged on every layout.
    override var intrinsicContentSize: NSSize {
        NSSize(width: max(1, runs.reduce(0) { $0 + width($1) }), height: 18)
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

/// One rail row (Ruling P3-R8), drawn rather than built from text fields: line 1 the state dot and the title, line 2
/// the state word, age and account. Each line is clipped to its rect and cut with "…" (RowLayout): nothing overlaps.
/// The table makes and lays out about thirty of these for a window, and one view that draws two strings is far cheaper
/// than a stack of NSTextFields (Ruling P3-R6's launch budget).
final class RowCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("row")
    private var session: Session?
    private var ageText = ""

    private static let leftStyle: NSParagraphStyle = {
        let p = NSMutableParagraphStyle()
        p.lineBreakMode = .byTruncatingTail
        return p
    }()

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
        let parts = RowLayout.frames(width: bounds.width, height: bounds.height)
        func line(_ c: RowColumn, _ text: NSAttributedString) {
            guard let r = parts[c] else { return }
            NSGraphicsContext.saveGraphicsState()
            r.clip()
            text.draw(with: r, options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
            NSGraphicsContext.restoreGraphicsState()
        }
        // fleet.html `.dot`: filled in the state's colour; waiting is a ring; done and ended are quiet and faded.
        if let d = parts[.dot] {
            let dot = NSBezierPath(ovalIn: d.insetBy(dx: 0.75, dy: 0.75))
            if s.state == .waiting {
                (on ?? Palette.quiet).setStroke()
                dot.lineWidth = 1.5
                dot.stroke()
            } else {
                (on ?? Palette.of(s.state)).withAlphaComponent(s.state.order >= 4 ? 0.7 : 1).setFill()
                dot.fill()
            }
        }
        let grey = on ?? NSColor.secondaryLabelColor
        line(.title, NSAttributedString(string: Words.title(s), attributes: [
            .font: Fonts.row, .foregroundColor: on ?? (s.live ? NSColor.labelColor : grey),
            .paragraphStyle: Self.leftStyle,
        ]))
        let info = NSMutableAttributedString(string: s.state.word, attributes: [
            .font: s.state.order <= 2 ? Fonts.word : Fonts.small, .foregroundColor: on ?? Palette.of(s.state),
            .paragraphStyle: Self.leftStyle,
        ])
        let rest = [ageText, s.account ?? ""].filter { !$0.isEmpty }
        if !rest.isEmpty {
            info.append(NSAttributedString(string: " · " + rest.joined(separator: " · "), attributes: [
                .font: Fonts.small, .foregroundColor: grey, .paragraphStyle: Self.leftStyle,
            ]))
        }
        line(.info, info)
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
        tally.clipsToBounds = true
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

    /// The name gives way first (cut with "…"), down to 60 pt; past that the tally is cut at the left of its room.
    /// The two never share a pixel.
    override func layout() {
        super.layout()
        let y = bounds.height - 22, inner = max(0, bounds.width - 12)
        let w = min(tally.intrinsicContentSize.width, max(0, inner - 60 - 12))
        tally.frame = NSRect(x: bounds.width - 6 - w, y: y, width: w, height: 18)
        name.frame = NSRect(x: 6, y: y + 1, width: max(0, inner - w - (w > 0 ? 12 : 0)), height: 16)
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
