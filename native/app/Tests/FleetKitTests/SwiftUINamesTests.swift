// Compile-only guard: FleetKit's names must not collide with SwiftUI's in a file that imports both (FleetApp's views
// do). A FleetKit `State` made `@State` ambiguous, and a `Group` shadowed `SwiftUI.Group`; if either comes back, this
// file stops compiling.

import FleetKit
import SwiftUI
import Testing

private struct NamesView: View {
    @State private var state: SessionState = .needsYou
    @State private var groups: [BoardGroup] = []

    var body: some View {
        Group {
            Text(state.word)
            ForEach(groups) { Text($0.name) }
        }
    }
}

@MainActor @Suite struct SwiftUINamesTests {
    @Test func fleetKitAndSwiftUINamesLiveTogether() {
        _ = NamesView()
    }
}
