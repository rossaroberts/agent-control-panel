// Where a rail row's parts go (Ruling P3-R8): two lines, the state dot and the title on the first, the state word, age
// and account on the second, both lines from the same left edge and cut with "…" at the right. Every part is a rect of
// its own, inside the row and apart from every other, and the cell clips each string to its rect: nothing draws over
// anything, at any rail width.

import CoreGraphics

public enum RowColumn: Int, CaseIterable, Sendable {
    case dot, title, info
}

public enum RowLayout {
    /// A rail row's height.
    public static let height: CGFloat = 40
    public static let margin: CGFloat = 10
    static let dot: CGFloat = 7
    static let textX: CGFloat = 24
    static let lineHeight: CGFloat = 16

    /// The rects in a row `width` wide and `height` high, in flipped coordinates (y down).
    public static func frames(width: CGFloat, height: CGFloat) -> [RowColumn: CGRect] {
        let top = max(0, (height - 2 * lineHeight) / 2)
        let textW = max(0, width - textX - margin)
        let rects: [RowColumn: CGRect] = [
            .dot: CGRect(x: margin, y: top + (lineHeight - dot) / 2, width: dot, height: dot),
            .title: CGRect(x: textX, y: top, width: textW, height: lineHeight),
            .info: CGRect(x: textX, y: top + lineHeight, width: textW, height: lineHeight),
        ]
        let row = CGRect(x: 0, y: 0, width: max(0, width), height: height)
        // Inside the row, always; a part with no room left is not shown at all.
        return rects.compactMapValues { r in
            let c = r.intersection(row)
            return c.isNull || c.width < 1 ? nil : c
        }
    }
}
