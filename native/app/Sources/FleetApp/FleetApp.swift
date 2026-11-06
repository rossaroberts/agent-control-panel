// Fleet.app: the board window, all AppKit (Ruling P3-R6). Its launch is gated against the machine's own floor, measured
// in the same bench run (Ruling P3-R7): the last board on screen within floor + 60 ms of process start, the live board
// within floor + 100 ms, the floor being this binary's `--floor` window. The absolute 120 / 200 ms are reported, not
// gated. In order: the cached board is read, the window is made at its final frame with the board in it, drawn and
// handed to the window server; only then does the BoardClient connect.
//
//   FleetApp                    the app
//   FleetApp --bench out.json   launch, time "last board on screen" and "live board on screen" from process start
//                               (each after the table's rows are laid out, drawn and flushed; os_signpost events in
//                               the "dev.fleet.app" subsystem mark the same points), write them as JSON, quit
//   FleetApp --floor out.json   the floor: one empty titled window, made, ordered front, drawn and flushed the same
//                               way, timed from process start ({"floorMs": …}), then quit. No board, no cache, no socket
//   FleetApp --shot out.png     once the live board is drawn, capture the window (toolbar included) to a PNG and quit:
//                               a screenshot that needs no Screen Recording permission
//
// The socket is fleetd's default (`~/Library/Application Support/Fleet/fleetd.sock`) or FLEETD_SOCK. The app never
// starts fleetd: with no socket the toolbar says "fleetd not running" and how to install it.

import AppKit
import Combine
import FleetKit
import os

@main
enum FleetMain {
    @MainActor static var delegate: AppDelegate?

    @MainActor static func main() {
        if let i = CommandLine.arguments.firstIndex(of: "--floor"), i + 1 < CommandLine.arguments.count {
            Floor.run(out: URL(fileURLWithPath: CommandLine.arguments[i + 1]))
        }
        let bench = Bench.fromArguments()  // its first mark is before NSApplication connects to the window server
        let app = NSApplication.shared
        bench?.mark("shared")
        let d = AppDelegate(bench: bench)
        delegate = d
        app.delegate = d
        app.setActivationPolicy(.regular)
        bench?.mark("policy")
        app.run()
    }
}

/// The socket fleetd listens on, as fleetd resolves it on macOS.
func socketURL() -> URL {
    if let p = ProcessInfo.processInfo.environment["FLEETD_SOCK"], !p.isEmpty { return URL(fileURLWithPath: p) }
    return FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/Fleet/fleetd.sock")
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let bench: Bench?
    private var model: BoardModel!
    private var client: BoardClient!
    private var window: NSWindow!
    private var boardView: BoardController!
    private var header: HeaderController!
    private let toolbar = BoardToolbar()
    private var watches: [AnyCancellable] = []
    private var ticker: Timer?

    init(bench: Bench?) {
        self.bench = bench
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        bench?.mark("willFinishLaunching")
        NSApp.mainMenu = mainMenu()
        launch()
    }

    /// Everything happens here, before AppKit finishes launching (its check-in with LaunchServices costs ~25 ms
    /// that the board need not wait for): the window, its first paint, then the connection.
    private func launch() {
        let board = Board()
        let cache = BoardCache()
        model = BoardModel(board: board, cache: cache, cached: cache.load())  // the last board, before connecting
        bench?.mark("cache read")

        boardView = BoardController(model: model)
        header = HeaderController()
        let split = NSSplitViewController()
        let side = NSSplitViewItem(sidebarWithViewController: boardView)
        side.minimumThickness = 300
        side.maximumThickness = 980
        side.canCollapse = true
        let detail = NSSplitViewItem(viewController: header)
        detail.minimumThickness = 200
        split.addSplitViewItem(side)
        split.addSplitViewItem(detail)

        // The window is made at its final frame (the saved one, else 1320 × 800) and the sidebar at its width before
        // the board goes in, so nothing is laid out twice.
        let saved = UserDefaults.standard.string(forKey: "NSWindow Frame FleetBoard").map { NSRectFromString($0) }
        let frame = saved.flatMap { $0.width > 100 ? $0 : nil }
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1320, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        if let frame { window.setFrame(frame, display: false) } else { window.center() }
        window.title = "Fleet"
        window.isReleasedWhenClosed = false
        window.toolbar = toolbar.toolbar
        window.toolbarStyle = .unified
        split.view.frame = NSRect(origin: .zero, size: window.contentRect(forFrameRect: window.frame).size)
        split.splitView.autosaveName = "FleetBoardSplit"
        if UserDefaults.standard.object(forKey: "NSSplitView Subview Frames FleetBoardSplit") == nil {
            split.splitView.setPosition(680, ofDividerAt: 0)
        }
        window.contentViewController = split
        if let frame { window.setFrame(frame, display: false) }  // the controller's size must not win over it
        window.setFrameAutosaveName("FleetBoard")
        bench?.mark("window made")

        // The toolbar's meters fit the room each side of the split has, now and on every resize.
        let fit = { [weak self, weak split] in
            guard let self, let split, split.splitView.arrangedSubviews.count == 2 else { return }
            let panes = split.splitView.arrangedSubviews
            self.toolbar.fit(sidebar: panes[0].frame.width, detail: panes[1].frame.width)
        }
        watches.append(
            NotificationCenter.default.publisher(for: NSSplitView.didResizeSubviewsNotification, object: split.splitView)
                .sink { _ in MainActor.assumeIsolated { fit() } })
        watches.append(
            NotificationCenter.default.publisher(for: NSWindow.didResizeNotification, object: window)
                .sink { _ in MainActor.assumeIsolated { fit() } })
        fit()
        boardView.onSelect = { [weak self] in self?.showChrome() }
        showChrome()
        window.makeKeyAndOrderFront(nil)
        bench?.mark("ordered front")
        NSApp.activate()
        bench?.mark("activated")
        // The first paint: the cached board (or the empty one) is on screen before anything connects.
        boardView.paint()
        bench?.firstPaint(cached: model.cachedAt != nil && !boardView.rows.isEmpty)
        if let bench {
            boardView.onApplied = { [weak self] cached in
                guard let self, !cached, !self.boardView.rows.isEmpty else { return }
                self.boardView.paint()
                bench.livePaint()
            }
        }

        header.build()
        client = BoardClient(board: board)
        let client = client!
        Task { @MainActor [weak model] in
            for await s in await client.statuses() { model?.status = s }
        }
        Task { await client.start(socket: socketURL()) }

        watches.append(model.$needsYou.sink { n in NSApp.dockTile.badgeLabel = n > 0 ? String(n) : nil })
        watches.append(model.$status.receive(on: RunLoop.main).sink { [weak self] _ in self?.showChrome() })
        // The ages ("5m") and the 48 h fold move with the clock; nothing else runs while the board is quiet.
        ticker = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.boardView.tick() }
        }
        ticker?.tolerance = 5
        if let i = CommandLine.arguments.firstIndex(of: "--shot"), i + 1 < CommandLine.arguments.count {
            // `--width <pt>`: the window that wide (the sidebar keeps 260 pt for the detail beside it), to check the
            // board at a narrow window.
            if let j = CommandLine.arguments.firstIndex(of: "--width"), j + 1 < CommandLine.arguments.count,
                let w = Double(CommandLine.arguments[j + 1])
            {
                // Not saved: Ross's own frame and sidebar width stay as he left them.
                window.setFrameAutosaveName("")
                split.splitView.autosaveName = nil
                var f = window.frame
                f.size.width = w
                window.setFrame(f, display: true)
                split.splitView.setPosition(max(300, w - 260), ofDividerAt: 0)
            }
            shoot(to: URL(fileURLWithPath: CommandLine.arguments[i + 1]), tries: 50)
        }
    }

    /// The toolbar, the title and the detail, from the model (each skips the work when nothing it shows changed).
    private func showChrome() {
        toolbar.show(meta: model.meta, machine: model.machine, status: model.status)
        let title = model.needsYou > 0 ? "Fleet (\(model.needsYou))" : "Fleet"
        if window.title != title { window.title = title }
        header.show(model.selected, cachedAt: model.cachedAt)
    }

    /// `--shot`: waits for the live board (at most `tries` × 200 ms), lets it settle, captures the window into a PNG.
    private func shoot(to url: URL, tries: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [self] in
            guard tries > 0, model.cachedAt != nil || model.groups.isEmpty else {
                // The first session selected, so the picture shows its header too.
                model.selection = model.groups.first?.rows.first?.key
                boardView.apply(cached: nil)
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [self] in
                    if let image = ownWindowImage(window.windowNumber) {
                        try? NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])?.write(to: url)
                    }
                    NSApp.terminate(nil)
                }
                return
            }
            shoot(to: url, tries: tries - 1)
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { window.makeKeyAndOrderFront(nil) }
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        model.cache?.flush()
    }

    private func mainMenu() -> NSMenu {
        let bar = NSMenu()
        func item(_ t: String, _ a: Selector?, _ k: String = "", _ mods: NSEvent.ModifierFlags = .command) -> NSMenuItem {
            let i = NSMenuItem(title: t, action: a, keyEquivalent: k)
            i.keyEquivalentModifierMask = mods
            return i
        }
        @discardableResult func menu(_ title: String, _ items: [NSMenuItem]) -> NSMenu {
            let m = NSMenu(title: title)
            items.forEach(m.addItem)
            let top = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            top.submenu = m
            bar.addItem(top)
            return m
        }
        menu("Fleet", [
            item("About Fleet", #selector(NSApplication.orderFrontStandardAboutPanel(_:))), .separator(),
            item("Hide Fleet", #selector(NSApplication.hide(_:)), "h"),
            item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]),
            item("Show All", #selector(NSApplication.unhideAllApplications(_:))), .separator(),
            item("Quit Fleet", #selector(NSApplication.terminate(_:)), "q"),
        ])
        menu("Edit", [
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ])
        menu("View", [
            item("Show Sidebar", #selector(NSSplitViewController.toggleSidebar(_:)), "s", [.command, .control]),
            item("Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control]),
        ])
        NSApp.windowsMenu = menu("Window", [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
            item("Zoom", #selector(NSWindow.performZoom(_:))),
            item("Close", #selector(NSWindow.performClose(_:)), "w"),
        ])
        return bar
    }
}

/// `--bench <out.json>`: the launch budgets, from process start to the board on screen, with the phases between.
@MainActor
final class Bench {
    private let out: URL
    private let log = OSLog(subsystem: "dev.fleet.app", category: .pointsOfInterest)
    private var lastBoardMs: Double?
    private var liveMs: Double?
    private var fromCache = false
    private var phases: [String: Double] = [:]
    private var done = false

    init(out: URL) {
        self.out = out
        mark("main")
        DispatchQueue.main.asyncAfter(deadline: .now() + 10) { [weak self] in self?.finish() }
    }

    static func fromArguments() -> Bench? {
        let a = CommandLine.arguments
        guard let i = a.firstIndex(of: "--bench"), i + 1 < a.count else { return nil }
        return Bench(out: URL(fileURLWithPath: a[i + 1]))
    }

    func mark(_ phase: String) {
        phases[phase] = Self.sinceProcessStart()
    }

    /// The window's first paint, the table drawn and flushed: the last board, when it showed the cache.
    func firstPaint(cached: Bool) {
        mark("first paint")
        fromCache = cached
        if cached {
            lastBoardMs = phases["first paint"]
            os_signpost(.event, log: log, name: "last board on screen")
        }
    }

    /// The first live render, the table drawn and flushed.
    func livePaint() {
        guard liveMs == nil else { return }
        liveMs = Self.sinceProcessStart()
        if lastBoardMs == nil { lastBoardMs = liveMs }  // no cache: the live board is the first one
        os_signpost(.event, log: log, name: "live board on screen")
        finish()
    }

    private func finish() {
        guard !done else { return }
        done = true
        let r = { (v: Double?) in v.map { ($0 * 100).rounded() / 100 } as Any }
        let json: [String: Any] = [
            "lastBoardMs": r(lastBoardMs), "liveMs": r(liveMs), "fromCache": fromCache,
            "phases": phases.mapValues { ($0 * 100).rounded() / 100 },
        ]
        if let data = try? JSONSerialization.data(withJSONObject: json, options: [.sortedKeys]) {
            try? data.write(to: out)
        }
        NSApp.terminate(nil)
    }

    /// Milliseconds since this process started (the kernel's start time of the pid, not the first line of main).
    static func sinceProcessStart() -> Double {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        guard sysctl(&mib, 4, &info, &size, nil, 0) == 0 else { return -1 }
        let start = info.kp_proc.p_starttime
        var now = timeval()
        gettimeofday(&now, nil)
        return Double(now.tv_sec - start.tv_sec) * 1000 + Double(now.tv_usec - start.tv_usec) / 1000
    }
}

/// `--floor`: what any AppKit app on this machine pays before its first window is on screen, measured by the same clock
/// as the bench: NSApplication, one empty titled window, ordered front, drawn, flushed.
@MainActor
final class Floor: NSObject, NSApplicationDelegate {
    private let out: URL
    private var window: NSWindow?

    private init(out: URL) {
        self.out = out
    }

    static func run(out: URL) -> Never {
        let app = NSApplication.shared
        let d = Floor(out: out)
        app.delegate = d
        app.setActivationPolicy(.regular)
        app.run()
        exit(0)
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        let w = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1320, height: 800), styleMask: [.titled, .closable, .resizable],
            backing: .buffered, defer: false)
        window = w
        w.makeKeyAndOrderFront(nil)
        w.displayIfNeeded()
        CATransaction.flush()
        let ms = Bench.sinceProcessStart()
        if let data = try? JSONSerialization.data(withJSONObject: ["floorMs": (ms * 100).rounded() / 100]) {
            try? data.write(to: out)
        }
        exit(0)
    }
}

/// The window as the window server composites it (vibrancy, toolbar and all). An app may read its own windows without
/// Screen Recording permission; `CGWindowListCreateImage` is obsoleted in the SDK but still served, so it is looked up
/// at run time. Used only by `--shot`.
private func ownWindowImage(_ number: Int) -> CGImage? {
    typealias Create = @convention(c) (CGRect, UInt32, UInt32, UInt32) -> Unmanaged<CGImage>?
    guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "CGWindowListCreateImage") else { return nil }
    let create = unsafeBitCast(sym, to: Create.self)
    // kCGWindowListOptionIncludingWindow = 1 << 3; kCGWindowImageBoundsIgnoreFraming = 1 << 0.
    return create(.null, 1 << 3, UInt32(number), 1 << 0)?.takeRetainedValue()
}
