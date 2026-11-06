// Fleet.app: the board window. An AppKit shell (NSApplication, one NSWindow, a standard menu bar) around a SwiftUI
// NavigationSplitView, because an AppKit launch gets the window on screen sooner than SwiftUI's App lifecycle and the
// board's budget is 50 ms from process start (spec §9.3). The cached board is read before anything connects and drawn
// with the window's first frame; the BoardClient then brings it live.
//
//   FleetApp                    the app
//   FleetApp --bench out.json   launch, time "last board on screen" and "live board on screen" from process start
//                               (os_signpost events in the "dev.fleet.app" subsystem mark the same points), write
//                               them as JSON, quit
//   FleetApp --shot out.png     once the live board is drawn, render the window (toolbar included) to a PNG and quit:
//                               a screenshot that needs no Screen Recording permission
//
// The socket is fleetd's default (`~/Library/Application Support/Fleet/fleetd.sock`) or FLEETD_SOCK. The app never
// starts fleetd: with no socket the toolbar says "fleetd not running" and how to install it.

import AppKit
import Combine
import FleetKit
import SwiftUI
import os

@main
enum FleetMain {
    @MainActor static var delegate: AppDelegate?

    @MainActor static func main() {
        let app = NSApplication.shared
        let d = AppDelegate(bench: Bench.fromArguments())
        delegate = d
        app.delegate = d
        app.setActivationPolicy(.regular)
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
    private var badge: AnyCancellable?
    private var ticker: Timer?

    init(bench: Bench?) {
        self.bench = bench
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = mainMenu()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let board = Board()
        let cache = BoardCache()
        // The last board, before anything connects.
        model = BoardModel(board: board, cache: cache, cached: cache.load())
        bench?.attach(model)

        // The window at its final style and frame first, then the board in it: SwiftUI lays the board out once.
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1320, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        window.title = "Fleet"
        window.toolbarStyle = .unified
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("FleetBoard")
        if !window.setFrameUsingName("FleetBoard") { window.center() }
        let host = NSHostingController(rootView: BoardView(model: model))
        host.sceneBridgingOptions = [.toolbars, .title]
        let frame = window.frame
        window.contentViewController = host
        window.setFrame(frame, display: false)  // the controller's preferred size must not win over the saved frame
        window.makeKeyAndOrderFront(nil)
        NSApp.activate()

        client = BoardClient(board: board)
        let client = client!
        Task { @MainActor [model] in
            for await s in await client.statuses() { model?.status = s }
        }
        Task { await client.start(socket: socketURL()) }

        badge = model.$needsYou.sink { n in NSApp.dockTile.badgeLabel = n > 0 ? String(n) : nil }
        // The ages ("5m") and the 48 h fold move with the clock; nothing else runs while the board is quiet.
        ticker = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.model.refresh() }
        }
        ticker?.tolerance = 5
        if let i = CommandLine.arguments.firstIndex(of: "--shot"), i + 1 < CommandLine.arguments.count {
            shoot(to: URL(fileURLWithPath: CommandLine.arguments[i + 1]), tries: 50)
        }
    }

    /// `--shot`: waits for the live board (at most `tries` × 200 ms), lets it settle, draws the window into a PNG.
    private func shoot(to url: URL, tries: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [self] in
            guard tries > 0, model.cachedAt != nil || model.groups.isEmpty else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [self] in
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
        func menu(_ title: String, _ items: [NSMenuItem]) {
            let m = NSMenu(title: title)
            items.forEach(m.addItem)
            let top = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            top.submenu = m
            bar.addItem(top)
        }
        func item(_ t: String, _ a: Selector?, _ k: String = "", _ mods: NSEvent.ModifierFlags = .command) -> NSMenuItem {
            let i = NSMenuItem(title: t, action: a, keyEquivalent: k)
            i.keyEquivalentModifierMask = mods
            return i
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
        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"))
        windowMenu.addItem(item("Zoom", #selector(NSWindow.performZoom(_:))))
        windowMenu.addItem(item("Close", #selector(NSWindow.performClose(_:)), "w"))
        let top = NSMenuItem(title: "Window", action: nil, keyEquivalent: "")
        top.submenu = windowMenu
        bar.addItem(top)
        NSApp.windowsMenu = windowMenu
        return bar
    }
}

/// `--bench <out.json>`: the launch budgets, from process start to the board on screen.
@MainActor
final class Bench {
    private let out: URL
    private let log = OSLog(subsystem: "dev.fleet.app", category: .pointsOfInterest)
    private var lastBoardMs: Double?
    private var liveMs: Double?
    private var hadCache = false

    init(out: URL) {
        self.out = out
    }

    static func fromArguments() -> Bench? {
        let a = CommandLine.arguments
        guard let i = a.firstIndex(of: "--bench"), i + 1 < a.count else { return nil }
        return Bench(out: URL(fileURLWithPath: a[i + 1]))
    }

    func attach(_ model: BoardModel) {
        hadCache = model.cachedAt != nil
        os_signpost(.event, log: log, name: "model ready")
        model.onRender = { [weak self] cached in
            guard let self, !model.groups.isEmpty else { return }
            // The render is in the model; it is on screen once the window has drawn and committed it, at the end of
            // this turn of the run loop. Force that now so the time is the time it was drawn.
            NSApp.windows.first?.displayIfNeeded()
            CATransaction.flush()
            let ms = Self.sinceProcessStart()
            if cached, self.lastBoardMs == nil {
                self.lastBoardMs = ms
                os_signpost(.event, log: self.log, name: "last board on screen")
            } else if !cached, self.liveMs == nil {
                self.liveMs = ms
                if self.lastBoardMs == nil { self.lastBoardMs = ms }  // no cache: the live board is the first one
                os_signpost(.event, log: self.log, name: "live board on screen")
                self.finish()
            }
        }
        // A cached board was drawn inside BoardModel's init, before the window existed: draw it again now it does.
        if hadCache {
            DispatchQueue.main.async { model.onRender?(true) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 10) { [weak self] in self?.finish() }
    }

    private func finish() {
        let json: [String: Any] = [
            "lastBoardMs": lastBoardMs.map { ($0 * 100).rounded() / 100 } as Any,
            "liveMs": liveMs.map { ($0 * 100).rounded() / 100 } as Any,
            "fromCache": hadCache,
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
