// The detail side, in AppKit: the selected session's header (title; dot, state word and reason; machine, account and
// the rest). Task 6 puts its terminal under it.

import AppKit
import FleetKit

@MainActor
final class HeaderController: NSViewController {
    private let stack = NSStackView()
    private let heading = label(.systemFont(ofSize: 17, weight: .regular), .labelColor)
    private let dot = DotView()
    private let word = label(Fonts.word)
    private let reason = label(Fonts.small)
    private let grid = NSGridView()
    private let none = label(.systemFont(ofSize: 13), align: .center)
    private var shown: (Session?, Date?)?

    /// An empty view at launch; the header is built after the first paint (`build()`), so it costs the launch nothing.
    override func loadView() { view = NSView() }

    private var built = false

    func build() {
        guard !built else { return }
        built = true
        let v = view
        for l in [heading, reason] { l.isSelectable = true }
        heading.lineBreakMode = .byTruncatingTail
        dot.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([dot.widthAnchor.constraint(equalToConstant: 7), dot.heightAnchor.constraint(equalToConstant: 7)])
        let state = NSStackView(views: [dot, word, reason])
        state.spacing = 8
        state.alignment = .centerY
        grid.rowSpacing = 4
        grid.columnSpacing = 16
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.addArrangedSubview(heading)
        stack.addArrangedSubview(state)
        stack.addArrangedSubview(grid)
        stack.translatesAutoresizingMaskIntoConstraints = false
        none.stringValue = "Select a session"
        none.translatesAutoresizingMaskIntoConstraints = false
        v.addSubview(stack)
        v.addSubview(none)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: v.safeAreaLayoutGuide.topAnchor, constant: 20),
            stack.leadingAnchor.constraint(equalTo: v.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(lessThanOrEqualTo: v.trailingAnchor, constant: -20),
            none.centerXAnchor.constraint(equalTo: v.centerXAnchor),
            none.centerYAnchor.constraint(equalTo: v.centerYAnchor),
        ])
        let pending = shown
        shown = nil
        show(pending?.0, cachedAt: pending?.1)
    }

    func show(_ s: Session?, cachedAt: Date?) {
        if let p = shown, p.0 == s, p.1 == cachedAt { return }
        shown = (s, cachedAt)
        guard built else { return }
        stack.isHidden = s == nil
        none.isHidden = s != nil
        guard let s else { return }
        heading.stringValue = Words.title(s)
        dot.state = s.state
        word.stringValue = s.state.word
        word.textColor = Palette.of(s.state)
        reason.stringValue = s.reason
        while grid.numberOfRows > 0 { grid.removeRow(at: 0) }
        let facts: [(String, String?)] = [
            ("machine", s.key.machine), ("account", s.account), ("profile", s.key.profile), ("folder", s.cwd),
            ("model", s.model.map { [$0, s.effort].compactMap { $0 }.joined(separator: " · ") }),
            ("context", s.contextTokens.map { _ in Words.tokens(s.contextTokens) }),
            ("last active", s.lastAt.map { Words.ago($0) == "now" ? "now" : "\(Words.ago($0)) ago" }),
            ("job", s.jobId), ("from", cachedAt.map(Words.time)),
        ]
        for (k, v) in facts {
            guard let v, !v.isEmpty else { continue }
            let key = label(Fonts.small), val = label(Fonts.small, .labelColor)
            key.stringValue = k
            val.stringValue = v
            val.lineBreakMode = .byTruncatingMiddle
            val.isSelectable = true
            grid.addRow(with: [key, val])
        }
    }
}
