// One session as one quiet monospace line, the columns of `public/fleet.html` `row()` (lines 1944-1975): the state as a
// dot and a word, the title, the machine, the account, its age, its effort and its context. Hovering shows the reason.
// The colours are fleet.html's `--blue / --orange / --red / --grey / --quiet` (lines 35-43), light and dark.

import AppKit
import FleetKit
import SwiftUI

enum Palette {
    private static func dynamic(_ light: UInt32, _ dark: UInt32) -> Color {
        Color(nsColor: NSColor(name: nil) { a in
            let v = a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(
                srgbRed: CGFloat(v >> 16 & 0xff) / 255, green: CGFloat(v >> 8 & 0xff) / 255,
                blue: CGFloat(v & 0xff) / 255, alpha: 1)
        })
    }

    static let blue = dynamic(0x0a72a3, 0x3fb6ee)
    static let orange = dynamic(0xa5560f, 0xef9a4c)
    static let red = dynamic(0xbf3f33, 0xec7468)
    static let green = dynamic(0x2b7d52, 0x5fbd8b)
    static let grey = dynamic(0x5f666b, 0x9ba2a7)
    static let quiet = dynamic(0x858c91, 0x6c7277)
    static let rule = dynamic(0xe2e5e7, 0x2c3033)

    static func of(_ s: SessionState) -> Color {
        switch s {
        case .working: blue
        case .needsYou: orange
        case .capped: red
        case .waiting: grey
        case .done, .ended, .unreachable: quiet
        }
    }

    /// fleet.html `level`: 90 % and over is red, 70 % and over orange.
    static func level(_ pct: Double) -> Color? { pct >= 90 ? red : pct >= 70 ? orange : nil }
}

enum Words {
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

    static func title(_ s: Session) -> String {
        s.title ?? s.name ?? s.jobId ?? "\(s.engine)"
    }

    static func time(_ d: Date) -> String {
        d.formatted(date: Calendar.current.isDateInToday(d) ? .omitted : .abbreviated, time: .shortened)
    }
}

extension Font {
    static let row = Font.system(size: 12, design: .monospaced)
    static let rowSmall = Font.system(size: 11, design: .monospaced)
}

/// fleet.html `.dot`: filled in the state's colour; waiting is a ring; done and ended are quiet and faded.
struct StateDot: View {
    let state: SessionState

    var body: some View {
        Group {
            if state == .waiting {
                Circle().strokeBorder(Palette.quiet, lineWidth: 1.5)
            } else {
                Circle().fill(Palette.of(state)).opacity(state.order >= 4 ? 0.7 : 1)
            }
        }
        .frame(width: 7, height: 7)
    }
}

/// The state word in its colour (fleet.html `.st`): bold-ish for the live states, plain for waiting and closed.
struct StateWord: View {
    let state: SessionState

    var body: some View {
        Text(state.word)
            .font(.system(size: 11, weight: state.order <= 2 ? .medium : .regular, design: .monospaced))
            .foregroundStyle(Palette.of(state))
    }
}

struct RowView: View {
    let session: Session
    /// From the cached board: drawn quieter until the live board replaces it.
    var cached = false

    var body: some View {
        let s = session
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            HStack(spacing: 6) {
                StateDot(state: s.state).alignmentGuide(.firstTextBaseline) { $0[.bottom] - 1 }
                StateWord(state: s.state)
            }
            .frame(width: 82, alignment: .leading)
            Text(Words.title(s))
                .font(.row)
                .foregroundStyle(s.live ? Color.primary : Color.secondary)
                .lineLimit(1).truncationMode(.tail)
                .frame(minWidth: 120, maxWidth: .infinity, alignment: .leading)
            cell(s.key.machine, width: 64)
            cell(s.account ?? "", width: 64)
            cell(Words.ago(s.lastAt ?? s.startedAt), width: 30, align: .trailing)
            cell(s.effort ?? "", width: 40)
            cell(Words.tokens(s.contextTokens), width: 40, align: .trailing)
        }
        .opacity(cached ? 0.6 : 1)
        .help(s.reason)
        .accessibilityElement(children: .combine)
    }

    private func cell(_ text: String, width: CGFloat, align: Alignment = .leading) -> some View {
        Text(text)
            .font(.rowSmall)
            .foregroundStyle(.secondary)
            .lineLimit(1).truncationMode(.tail)
            .frame(width: width, alignment: align)
    }
}
