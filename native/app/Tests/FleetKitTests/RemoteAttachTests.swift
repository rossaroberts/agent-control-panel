// A session of another machine opens through that machine's own fleetd, and not while it is out of reach.

import Foundation
import Testing

@testable import FleetKit

@Suite struct RemoteAttachTests {
    /// A remote machine's job opens through its own fleetd (the caller names its machine as local); while it cannot be
    /// reached it says so instead of offering a terminal.
    @Test func aRemoteJobOpensAndAnUnreachableOneSaysWhy() {
        var s = Session(
            key: SessionKey(machine: "m2", profile: "p", sessionId: "c"), engine: .claude, name: nil, cwd: "/w",
            state: .working, reason: "fixture", live: true, pid: nil, stage: nil, model: nil, effort: nil,
            contextTokens: nil, lastAt: nil, said: nil, doing: nil, lastAsk: nil, firstAsk: nil, startedAt: nil,
            hookAt: nil, writtenAt: nil, jobId: nil, title: "c", epic: nil, archived: false, account: nil)
        s.jobId = "0a1b2c3d"
        #expect(Attach.of(s, local: "m2") == .open(.claude(profile: "p", job: "0a1b2c3d")))
        guard case .none = Attach.of(s, local: "m1") else {
            Issue.record("a machine with no endpoint opens nothing")
            return
        }
        let u = MergedBoard.unreachable(s, since: Date(timeIntervalSince1970: 0))
        #expect(Attach.of(u, local: "m2") == .none("m2 cannot be reached now: its terminal opens here when it is back."))
    }
}
