// The toolbar: the accounts' limit meters (fleet.html `renderAccounts`, lines 719-735: the window, a thin line, the
// percent), the machine's health (fleet.html `vitals`, lines 1028-1037: dev, db, cpu, mem, disk) and the connection
// ("live", "reconnecting…", "fleetd not running", or the daemon's refusal).

import FleetKit
import SwiftUI

/// The sidebar's half of the toolbar: the accounts' meters, over the rows they are about.
struct AccountsToolbar: ToolbarContent {
    @ObservedObject var model: BoardModel

    var body: some ToolbarContent {
        if !model.meta.accounts.isEmpty {
            ToolbarItem(placement: .automatic) {
                Accounts(accounts: model.meta.accounts)
            }
        }
    }
}

/// The detail's half: the machine's health and the connection.
struct BoardToolbar: ToolbarContent {
    @ObservedObject var model: BoardModel

    var body: some ToolbarContent {
        if model.machine != nil || model.meta.health != nil {
            ToolbarItem(placement: .primaryAction) {
                Vitals(machine: model.machine, health: model.meta.health)
            }
        }
        ToolbarItem(placement: .primaryAction) {
            Connection(status: model.status)
        }
    }
}

/// A plan limit as fleet.html draws it: `5h ▁▁▁ 42%`.
struct Meter: View {
    let limit: Limit

    var body: some View {
        let pct = Double(limit.usedPct)
        HStack(alignment: .center, spacing: 5) {
            Text(limit.window).foregroundStyle(.secondary)
            ZStack(alignment: .leading) {
                Capsule().fill(Palette.rule)
                Capsule().fill(Palette.level(pct) ?? Palette.grey).frame(width: 28 * min(100, pct) / 100)
            }
            .frame(width: 28, height: 2)
            Text("\(limit.usedPct)%").foregroundStyle(Palette.level(pct) ?? .secondary)
        }
        .help(limit.resetsAt.map { "resets in \(Words.until($0))" } ?? "")
    }
}

struct Accounts: View {
    let accounts: [Account]

    var body: some View {
        HStack(spacing: 14) {
            ForEach(accounts, id: \.name) { a in
                HStack(spacing: 6) {
                    Text(a.name)
                    if a.limits.isEmpty {
                        Text("·").foregroundStyle(.secondary).help("no reading yet")
                    } else {
                        ForEach(a.limits, id: \.window) { Meter(limit: $0) }
                    }
                }
                .help("\(a.name) · \(String(describing: a.engine)) · \(a.profile)")
            }
        }
        .font(.rowSmall)
        .fixedSize()
    }
}

struct Vitals: View {
    let machine: String?
    let health: Health?

    var body: some View {
        HStack(spacing: 12) {
            if let machine { Text(machine).foregroundStyle(.secondary) }
            if let h = health {
                vital("dev", "\(h.dev)", nil, "dev servers")
                vital("db", "\(h.db)", nil, "database stacks")
                vital("cpu", "\(h.cpuPct)%", Double(h.cpuPct))
                vital("mem", "\(h.memPct)%", Double(h.memPct))
                vital("disk", "\(h.diskPct)%", Double(h.diskPct))
            }
        }
        .font(.rowSmall)
        .fixedSize()
    }

    private func vital(_ k: String, _ v: String, _ pct: Double?, _ help: String = "") -> some View {
        HStack(spacing: 4) {
            Text(k).foregroundStyle(.secondary)
            Text(v).foregroundStyle(pct.flatMap(Palette.level) ?? .primary)
        }
        .help(help)
    }
}

struct Connection: View {
    let status: BoardClient.Status

    var body: some View {
        let (word, colour, help): (String, Color, String) =
            switch status {
            case .live: ("live", Palette.green, "Connected to fleetd")
            case .reconnecting: ("reconnecting…", Palette.orange, "The connection to fleetd was lost; trying again")
            case .absent:
                ("fleetd not running", Palette.red, "No fleetd at its socket. Install it with: fleetd install-service")
            case let .refused(why): ("fleetd refused", Palette.red, why)
            }
        HStack(spacing: 5) {
            Circle().fill(colour).frame(width: 6, height: 6)
            Text(word)
        }
        .padding(.leading, 8)
        .font(.rowSmall)
        .help(help)
        .fixedSize()
    }
}
