// Where a session row's columns go at a given width: the state (dot and word) and the title always, then from the right
// context, effort, age, account and machine. The title takes what is left; when that is under `minTitle`, columns are
// dropped in a fixed order (context, effort, machine, then age) until it fits. Every column is a rect of its own,
// inside the row and apart from every other, and the cell clips each string to its rect: nothing draws over anything.

import CoreGraphics

public enum RowColumn: Int, CaseIterable, Sendable {
    case dot, state, title, machine, account, age, effort, context
}

public enum RowLayout {
    public static let margin: CGFloat = 6
    public static let gap: CGFloat = 10
    public static let minTitle: CGFloat = 120
    /// The right-hand columns, rightmost first, with their widths.
    static let right: [(RowColumn, CGFloat)] = [(.context, 40), (.effort, 54), (.age, 32), (.account, 64), (.machine, 64)]
    /// The order they give way in when the title would be too narrow.
    public static let dropOrder: [RowColumn] = [.context, .effort, .machine, .age]

    /// The rects of the columns shown in a row `width` wide and `height` high (flipped or not: only x varies).
    public static func frames(width: CGFloat, height: CGFloat) -> [RowColumn: CGRect] {
        let titleX: CGFloat = 101
        func layout(_ shown: Set<RowColumn>) -> (rects: [RowColumn: CGRect], title: CGFloat) {
            var out: [RowColumn: CGRect] = [:]
            var x = width - margin
            for (c, w) in right where shown.contains(c) {
                x -= w
                out[c] = CGRect(x: x, y: 0, width: w, height: height)
                x -= gap
            }
            return (out, x - titleX)
        }
        var shown = Set(right.map(\.0))
        var (rects, title) = layout(shown)
        for c in dropOrder where title < minTitle {
            shown.remove(c)
            (rects, title) = layout(shown)
        }
        // Narrower still: the account gives way too, rather than ever overlap the title.
        if title < minTitle / 2 {
            shown.remove(.account)
            (rects, title) = layout(shown)
        }
        let row = CGRect(x: 0, y: 0, width: max(0, width), height: height)
        rects[.dot] = CGRect(x: margin, y: 0, width: 7, height: height)
        rects[.state] = CGRect(x: 19, y: 0, width: 74, height: height)
        rects[.title] = CGRect(x: titleX, y: 0, width: max(0, title), height: height)
        // Inside the row, always; a column with no room left is not shown at all.
        return rects.compactMapValues { r in
            let c = r.intersection(row)
            return c.isNull || c.width < 1 ? nil : c
        }
    }
}
