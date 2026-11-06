// Fleet.app: the rail of sessions and the terminal pane (Ruling P3-R8), all AppKit (Ruling P3-R6). Its launch is gated against the machine's own floor, measured
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
//   FleetApp --shot out.png [--width <pt>] [--select <session or job id>]
//                               once the live board is drawn (and the selected session's terminal has drawn), capture
//                               the window (toolbar included) to a PNG and quit: a screenshot that needs no Screen
//                               Recording permission. Nothing is selected without --select
//   FleetApp --ui-bench out.json, --live-switch A B out.json   the interaction budgets (UIBench.swift)
//
// The socket is fleetd's default (`~/Library/Application Support/Fleet/fleetd.sock`) or FLEETD_SOCK. The app never
// starts fleetd: with no socket the toolbar says "fleetd not running" and how to install it. Other machines (Ruling
// A-R1) are on the same board over their fleetd's TCP listener (MachineList: the Tailscale peers by default, or the
// Machines menu's list).

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
    private let uiBench = UIBench.fromArguments()
    private var model: BoardModel!
    /// This Mac's fleetd and every other machine's (Ruling A-R1); nil in the synthetic bench.
    private var machines: Machines?
    private var machineList: MachineList?
    private var localEndpoint: Endpoint?
    private var backend: TermBackend!
    private var window: NSWindow!
    private var split: NSSplitViewController!
    private var boardView: BoardController!
    private var pane: TerminalPaneController!
    private var nav: Navigator!
    private let toolbar = BoardToolbar()
    private var watches: [AnyCancellable] = []
    private var ticker: Timer?

    /// The rail's width range and default (Ruling P3-R8).
    static let railMin: CGFloat = 320, railMax: CGFloat = 420, railDefault: CGFloat = 360

    init(bench: Bench?) {
        self.bench = bench
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        bench?.mark("willFinishLaunching")
        launch()
    }

    /// A run that must leave Ross's saved window frame and rail width as they are.
    private var measuring: Bool {
        uiBench != nil || LiveSwitch.arguments() != nil || TermCheck.arguments() != nil
            || CommandLine.arguments.contains("--width")
    }

    /// Everything happens here, before AppKit finishes launching (its check-in with LaunchServices costs ~25 ms
    /// that the board need not wait for): the window, its first paint, then the connections.
    private func launch() {
        // Shots only: the appearance to draw in (FLEET_APPEARANCE=dark|light), to check both without changing the Mac's.
        switch ProcessInfo.processInfo.environment["FLEET_APPEARANCE"] {
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        default: break
        }
        let board = Board()
        if uiBench != nil {
            model = BoardModel(board: board)  // synthetic: no cache, no fleetd
            model.status = .live
            backend = EchoBackend()
        } else {
            // Every machine's last board, before connecting: this Mac's from its cache file as before, each other
            // machine's from its own (MergedBoard.cacheURL), all merged into one board.
            let local = Endpoint.unix(socketURL())
            let list = MachineList()
            let machines = Machines(
                endpoints: [local] + list.endpoints,
                cacheURL: { $0 == local ? BoardCache.defaultURL : MergedBoard.cacheURL(for: $0) })
            self.machines = machines
            machineList = list
            localEndpoint = local
            model = BoardModel(merged: machines.merged, cachedAt: machines.cachedAt)
            let live = LiveBackend { [weak machines] machine in machines?.node(for: machine)?.terms }
            backend = live
            watchTerminals()
        }
        bench?.mark("cache read")

        boardView = BoardController(model: model)
        pane = TerminalPaneController(model: model, backend: backend)
        split = NSSplitViewController()
        let side = NSSplitViewItem(sidebarWithViewController: boardView)
        side.minimumThickness = Self.railMin
        side.maximumThickness = Self.railMax
        side.canCollapse = true
        let detail = NSSplitViewItem(viewController: pane)
        detail.minimumThickness = 300
        split.addSplitViewItem(side)
        split.addSplitViewItem(detail)
        nav = Navigator(model: model, rail: boardView, pane: pane)
        nav.split = split

        // The window is made at its final frame (the saved one, else 1320 × 800) and the rail at its width before
        // the board goes in, so nothing is laid out twice.
        let saved = measuring ? nil : UserDefaults.standard.string(forKey: "NSWindow Frame FleetBoard").map { NSRectFromString($0) }
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
        if !measuring { split.splitView.autosaveName = "FleetRail" }
        if measuring || UserDefaults.standard.object(forKey: "NSSplitView Subview Frames FleetRail") == nil {
            split.splitView.setPosition(Self.railDefault, ofDividerAt: 0)
        }
        window.contentViewController = split
        if let frame { window.setFrame(frame, display: false) }  // the controller's size must not win over it
        if !measuring { window.setFrameAutosaveName("FleetBoard") }
        bench?.mark("window made")

        // The toolbar's meters fit the room each side of the split has, now and on every resize.
        let fit = { [weak self] in
            guard let self, let split = self.split, split.splitView.arrangedSubviews.count == 2 else { return }
            let panes = split.splitView.arrangedSubviews
            let rail = split.splitViewItems[0].isCollapsed ? 0 : panes[0].frame.width
            self.toolbar.fit(sidebar: rail, detail: panes[1].frame.width)
            if self.nav.switcherOpen { self.nav.switcher.place() }
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
        window.makeFirstResponder(boardView.table)

        // Once the live board is on screen (or after 300 ms, if fleetd does not answer): the menus and the keys
        // (5–15 ms, measured), the pane's message line, the switcher and the text input system made ready. None of it
        // is on screen at launch, so none of it holds the board back.
        model.onRender { [weak self] cached in
            if !cached { DispatchQueue.main.async { self?.afterLaunch() } }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in self?.afterLaunch() }

        if let uiBench, let echo = backend as? EchoBackend {
            _ = board.apply(.snapshot(run: 1, seq: 0, machine: UIBench.machine, sessions: UIBench.sessions()))
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) { [self] in
                Task { await uiBench.run(window: window, model: model, nav: nav, pane: pane, backend: echo) }
            }
            return
        }

        if let machines {
            machines.onStatus = { [weak self] node, _ in
                guard let self, let machines = self.machines else { return }
                // The board's own words ("fleetd not running") are this Mac's; every machine is in the toolbar.
                if node === machines.nodes.first { self.model.status = node.status }
                self.showChrome()
            }
            watches.append(machines.merged.$machines.receive(on: RunLoop.main).sink { [weak self] _ in self?.showChrome() })
            machineList?.nameOf = { [weak machines] e in machines?.node(for: e)?.source.machine }
            machineList?.duplicateOf = { [weak machines] e in
                guard let machines, let n = machines.node(for: e) else { return nil }
                return machines.duplicate(n)
            }
            machineList?.onChange = { [weak self] in self?.machinesChanged() }
            machines.start()
        }

        watches.append(model.$needsYou.sink { n in NSApp.dockTile.badgeLabel = n > 0 ? String(n) : nil })
        watches.append(model.$status.receive(on: RunLoop.main).sink { [weak self] _ in self?.showChrome() })
        // The ages ("5m") and the 48 h fold move with the clock; nothing else runs while the board is quiet.
        ticker = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.boardView.tick() }
        }
        ticker?.tolerance = 5
        if let (job, secs, out) = TermCheck.arguments() {
            Task { await TermCheck.run(job: job, seconds: secs, out: out, model: model, pane: pane) }
        }
        if let (a, b, out) = LiveSwitch.arguments(), let live = backend as? LiveBackend {
            Task { await LiveSwitch.run((a, b), out: out, window: window, model: model, nav: nav, pane: pane, backend: live) }
        }
        if let i = CommandLine.arguments.firstIndex(of: "--shot"), i + 1 < CommandLine.arguments.count {
            // `--width <pt>`: the window that wide (the rail at its default), to check the layout at a given width.
            if let j = CommandLine.arguments.firstIndex(of: "--width"), j + 1 < CommandLine.arguments.count,
                let w = Double(CommandLine.arguments[j + 1])
            {
                var f = window.frame
                f.size.width = w
                window.setFrame(f, display: true)
                split.splitView.setPosition(Self.railDefault, ofDividerAt: 0)
            }
            // `--select <session or job id>`: that session open in the pane (the keyboard stays in the rail).
            let select = CommandLine.arguments.firstIndex(of: "--select").flatMap {
                $0 + 1 < CommandLine.arguments.count ? CommandLine.arguments[$0 + 1] : nil
            }
            shoot(to: URL(fileURLWithPath: CommandLine.arguments[i + 1]), select: select, tries: 50)
        }
    }

    private var launched = false

    private func afterLaunch() {
        guard !launched else { return }
        launched = true
        NSApp.mainMenu = nav.mainMenu()
        if let machineList, let bar = NSApp.mainMenu {
            bar.insertItem(machineList.menuItem(), at: max(0, bar.items.count - 1))  // before Window
            machineList.discover()  // the tailnet's peers, off the main thread; the board follows if they changed
        }
        nav.installKeys()
        pane.build()
        nav.prepare()
    }

    /// The toolbar, the title and the pane's header, from the model (each skips the work when nothing it shows
    /// changed).
    /// The Machines list changed (Settings, or the tailnet's peers): the board's machines follow it, this Mac first.
    private func machinesChanged() {
        guard let machines, let machineList, let localEndpoint else { return }
        machines.set(endpoints: [localEndpoint] + machineList.endpoints)
        watchTerminals()
        showChrome()
    }

    /// The pane hears of each machine's Terminals connection, by its client (known before the machine has a name).
    private func watchTerminals() {
        guard let machines, let live = backend as? LiveBackend else { return }
        for n in machines.nodes { live.watch(n.terms) }
    }

    private func showChrome() {
        // A machine not yet reached has no name from its board, and a duplicate has another's: the Machines list's
        // name (its Tailscale host name) stands in. Lines and nodes are paired by their source board (M4).
        var lines = machines?.merged.machines ?? []
        if let machines, let machineList {
            for (i, src) in machines.merged.sources.enumerated() where lines.indices.contains(i) {
                guard lines[i].machine.isEmpty || lines[i].duplicateOf != nil,
                    let n = machines.nodes.first(where: { $0.source === src })
                else { continue }
                lines[i].machine = machineList.entries.first { $0.endpoint == n.endpoint }?.name ?? n.endpoint.description
            }
        }
        toolbar.show(meta: model.meta, machine: model.machine, status: model.status, lines: lines)
        let title = model.needsYou > 0 ? "Fleet (\(model.needsYou))" : "Fleet"
        if window.title != title { window.title = title }
        pane.update(model.selected)
    }

    /// `--shot`: waits for the live board (at most `tries` × 200 ms), opens `select` if given and waits for its
    /// terminal to draw, lets it settle, captures the window into a PNG.
    private func shoot(to url: URL, select: String?, tries: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [self] in
            guard tries > 0, model.board.run == nil || model.cachedAt != nil else {
                var wait = 1.5
                if let select, let k = LiveSwitch.find(select, in: model) {
                    nav.open(k, focus: false)
                    wait = 4
                }
                DispatchQueue.main.asyncAfter(deadline: .now() + wait) { [self] in
                    if let (png, viewDrawn) = windowPNG(window, rail: boardView) {
                        try? png.write(to: url)
                        FileHandle.standardError.write(Data("shot: \(viewDrawn ? "view-drawn (screen locked?)" : "window server")\n".utf8))
                    }
                    NSApp.terminate(nil)
                }
                return
            }
            shoot(to: url, select: select, tries: tries - 1)
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { window.makeKeyAndOrderFront(nil) }
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        model.cache?.flush()
        machines?.flush()
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

/// The window as a PNG: as the window server composites it, or, when that comes back blank (a locked screen gives
/// every app a blank capture), as its own views draw it. A view-drawn capture cannot draw AppKit's materials (the
/// sidebar's glass comes out empty, the selection black) nor Metal layers, so for it the rail is drawn opaque and the
/// selection as a plain fill (`RailRowView.plain`), and the terminal should run with FLEET_TERM_RENDERER=cg.
@MainActor func windowPNG(_ window: NSWindow, rail: BoardController? = nil) -> (png: Data, viewDrawn: Bool)? {
    if let image = ownWindowImage(window.windowNumber), !blank(image),
        let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
    {
        return (png, false)
    }
    guard let frame = window.contentView?.superview, let rep = frame.bitmapImageRepForCachingDisplay(in: frame.bounds)
    else { return nil }
    rail?.plainForCapture(true)
    frame.cacheDisplay(in: frame.bounds, to: rep)
    rail?.plainForCapture(false)
    return rep.representation(using: .png, properties: [:]).map { ($0, true) }
}

/// Every sampled pixel the same: whole 32-bit pixels compared (one byte of each would be alpha, the same for every
/// opaque window), across the rows the image's own stride gives.
func blank(_ image: CGImage) -> Bool {
    guard image.bitsPerPixel == 32, let data = image.dataProvider?.data, let p = CFDataGetBytePtr(data) else {
        return false  // not a layout we can read: never call it blank
    }
    let row = image.bytesPerRow, w = image.width, h = image.height
    guard w > 0, h > 0, CFDataGetLength(data) >= row * h else { return false }
    func pixel(_ x: Int, _ y: Int) -> UInt32 {
        let o = y * row + x * 4
        return UInt32(p[o]) | UInt32(p[o + 1]) << 8 | UInt32(p[o + 2]) << 16 | UInt32(p[o + 3]) << 24
    }
    let first = pixel(0, 0)
    for y in stride(from: 0, to: h, by: max(1, h / 64)) {
        for x in stride(from: 0, to: w, by: max(1, w / 64)) where pixel(x, y) != first { return false }
    }
    return true
}

/// The window as the window server composites it (vibrancy, toolbar and all). An app may read its own windows without
/// Screen Recording permission; `CGWindowListCreateImage` is obsoleted in the SDK but still served, so it is looked up
/// at run time. Used only by `--shot`.
func ownWindowImage(_ number: Int) -> CGImage? {
    typealias Create = @convention(c) (CGRect, UInt32, UInt32, UInt32) -> Unmanaged<CGImage>?
    guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "CGWindowListCreateImage") else { return nil }
    let create = unsafeBitCast(sym, to: Create.self)
    // kCGWindowListOptionIncludingWindow = 1 << 3; kCGWindowImageBoundsIgnoreFraming = 1 << 0.
    return create(.null, 1 << 3, UInt32(number), 1 << 0)?.takeRetainedValue()
}
