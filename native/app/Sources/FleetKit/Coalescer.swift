// Renders coalesced to at most one per display frame (plan 3 Task 5, Review Focus 3): any number of changes between
// two frames cost one render, which reads the board as it is by then. A main-actor throttle, not a display link: the
// first change after a quiet spell renders on the next turn of the run loop (no wait for a frame boundary), and an
// idle board costs nothing at all, as no timer runs between changes.

import Foundation

@MainActor
public final class Coalescer {
    /// One display frame at 60 Hz.
    public static let frame: Duration = .nanoseconds(16_666_667)

    private let interval: Duration
    private let render: @MainActor () -> Void
    private var pending = false
    private var last: ContinuousClock.Instant?

    public init(interval: Duration = Coalescer.frame, render: @escaping @MainActor () -> Void) {
        self.interval = interval
        self.render = render
    }

    /// Something changed: render once, as soon as a frame has passed since the last render.
    public func mark() {
        guard !pending else { return }
        pending = true
        let wait = last.map { interval - (ContinuousClock.now - $0) } ?? .zero
        Task { @MainActor [weak self] in
            if wait > .zero { try? await Task.sleep(for: wait) }
            guard let self else { return }
            self.pending = false
            self.last = .now
            self.render()
        }
    }
}
