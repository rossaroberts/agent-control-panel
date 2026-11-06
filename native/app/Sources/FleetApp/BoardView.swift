// The board window: a standard NavigationSplitView. The sidebar is the board as `public/fleet.html` groups it (Needs
// you, the epics in fleet.json order, your groups, Other; BoardModel), each group a section with its tally, and the
// done or ended rows older than 48 h behind an always-visible "N older" line. The detail is the selected session's
// header (Task 6 puts its terminal under it). The selection is a SessionKey, so a row keeps it wherever it moves.

import FleetKit
import SwiftUI

struct BoardView: View {
    @ObservedObject var model: BoardModel
    /// The groups whose older rows are open.
    @State private var openOlder: Set<String> = []

    var body: some View {
        NavigationSplitView {
            List(selection: $model.selection) {
                if let at = model.cachedAt {
                    Text("from \(Words.time(at))")
                        .font(.rowSmall)
                        .foregroundStyle(.secondary)
                        .help("The board as it was at \(Words.time(at)), until fleetd answers")
                        .selectionDisabled()
                }
                ForEach(model.groups) { g in
                    Section {
                        ForEach(g.rows, id: \.key) { s in
                            RowView(session: s, cached: model.cachedAt != nil).tag(s.key)
                        }
                        if !g.older.isEmpty { older(g) }
                    } header: {
                        GroupHeader(group: g)
                    }
                }
            }
            .listStyle(.sidebar)
            .overlay { if model.groups.isEmpty { EmptyBoard(status: model.status) } }
            .navigationSplitViewColumnWidth(min: 540, ideal: 680, max: 980)
            .toolbar { AccountsToolbar(model: model) }
        } detail: {
            SessionHeader(session: model.selected, cachedAt: model.cachedAt)
        }
        .toolbar { BoardToolbar(model: model) }
        .navigationTitle(model.needsYou > 0 ? "Fleet (\(model.needsYou))" : "Fleet")
        .frame(minWidth: 820, minHeight: 420)
    }

    @ViewBuilder private func older(_ g: BoardGroup) -> some View {
        let open = openOlder.contains(g.id)
        Button {
            if open { openOlder.remove(g.id) } else { openOlder.insert(g.id) }
        } label: {
            Text("\(open ? "▾" : "▸") \(g.older.count) older")
                .font(.rowSmall)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Done or ended more than 48 hours ago")
        .selectionDisabled()
        if open {
            ForEach(g.older, id: \.key) { s in
                RowView(session: s, cached: model.cachedAt != nil).tag(s.key)
            }
        }
    }
}

/// A group's name and its tally of live rows (fleet.html `tally`: "2 working  1 waiting").
struct GroupHeader: View {
    let group: BoardGroup

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(group.name)
            Spacer(minLength: 8)
            ForEach([SessionState.needsYou, .working, .capped, .waiting], id: \.self) { st in
                let n = group.rows.filter { $0.live && $0.state == st }.count
                if n > 0 {
                    Text("\(n) \(st.word)").foregroundStyle(Palette.of(st))
                }
            }
        }
        .font(.rowSmall)
    }
}

/// What the detail side shows for now: the selected session's header.
struct SessionHeader: View {
    let session: Session?
    let cachedAt: Date?

    var body: some View {
        if let s = session {
            VStack(alignment: .leading, spacing: 10) {
                Text(Words.title(s))
                    .font(.title2)
                    .textSelection(.enabled)
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    StateDot(state: s.state)
                    StateWord(state: s.state)
                    Text(s.reason)
                        .font(.rowSmall)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 4) {
                    fact("machine", s.key.machine)
                    fact("account", s.account)
                    fact("profile", s.key.profile)
                    fact("folder", s.cwd)
                    fact("model", s.model.map { [$0, s.effort].compactMap { $0 }.joined(separator: " · ") })
                    fact("context", s.contextTokens.map { _ in Words.tokens(s.contextTokens) })
                    fact("last active", s.lastAt.map { Words.ago($0) == "now" ? "now" : "\(Words.ago($0)) ago" })
                    fact("job", s.jobId)
                    if let at = cachedAt { fact("from", Words.time(at)) }
                }
                .font(.rowSmall)
                Spacer()
            }
            .padding(20)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .navigationTitle(Words.title(s))
        } else {
            Text("Select a session")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder private func fact(_ k: String, _ v: String?) -> some View {
        if let v, !v.isEmpty {
            GridRow {
                Text(k).foregroundStyle(.secondary)
                Text(v).textSelection(.enabled).lineLimit(1).truncationMode(.middle)
            }
        }
    }
}

/// An empty board: why, in words (the app never starts fleetd itself).
struct EmptyBoard: View {
    let status: BoardClient.Status

    var body: some View {
        switch status {
        case .absent:
            ContentUnavailableView {
                Label("fleetd not running", systemImage: "bolt.horizontal.circle")
            } description: {
                Text("Fleet shows what fleetd sees. Install it as a login service with\n`fleetd install-service`")
            }
        case let .refused(why):
            ContentUnavailableView("fleetd refused the connection", systemImage: "exclamationmark.triangle",
                                   description: Text(why))
        case .reconnecting:
            ProgressView().controlSize(.small)
        case .live:
            Text("No sessions").foregroundStyle(.secondary)
        }
    }
}
