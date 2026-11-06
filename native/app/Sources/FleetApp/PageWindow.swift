// One window per page, each with a WKWebView made only when the window is first opened (lazy: launch pays nothing,
// P3-R7). ⌘R and ⌘⌥← / ⌘⌥→ act only inside a page window (⌘[ ⌘] are the board's). The window remembers its frame.

import AppKit
import WebKit

/// Reload and history keys that belong to a page window alone.
final class PageWindow: NSWindow {
    var onKey: ((Action) -> Bool)?
    enum Action { case reload, back, forward }

    override func performKeyEquivalent(with e: NSEvent) -> Bool {
        let mods = e.modifierFlags.intersection([.command, .shift, .control, .option])
        if mods == .command, e.charactersIgnoringModifiers?.lowercased() == "r", onKey?(.reload) == true { return true }
        if mods == [.command, .option] {
            switch e.keyCode {
            case 123: if onKey?(.back) == true { return true }
            case 124: if onKey?(.forward) == true { return true }
            default: break
            }
        }
        return super.performKeyEquivalent(with: e)
    }
}

@MainActor
final class PageWindowController: NSObject, WKNavigationDelegate {
    let page: Page
    private(set) var window: PageWindow?
    private(set) var webView: WKWebView?
    private var showingOffline = false
    /// A login just typed with "Remember" ticked: kept in the keychain once the page loads (a refused one is dropped).
    private var pending: (user: String, password: String, space: URLProtectionSpace)?

    init(page: Page) {
        self.page = page
    }

    func show() {
        if window == nil { build() }
        window?.makeKeyAndOrderFront(nil)
        NSApp.activate()
    }

    private func build() {
        let w = PageWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1100, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = page.name
        w.isReleasedWhenClosed = false
        w.center()
        w.setFrameAutosaveName("FleetPage-\(page.name)")  // after center(): a saved frame wins
        let cfg = WKWebViewConfiguration()
        let v = WKWebView(frame: w.contentLayoutRect, configuration: cfg)
        v.autoresizingMask = [.width, .height]
        v.navigationDelegate = self
        v.allowsBackForwardNavigationGestures = true
        w.contentView = v
        w.onKey = { [weak self] a in self?.handle(a) ?? false }
        window = w
        webView = v
        load()
    }

    private func load() {
        guard let u = page.address, let v = webView else { return }
        showingOffline = false
        v.load(URLRequest(url: u))
    }

    private func handle(_ a: PageWindow.Action) -> Bool {
        guard let v = webView else { return false }
        switch a {
        case .reload:
            if showingOffline || v.url == nil { load() } else { v.reload() }
        case .back: if v.canGoBack { v.goBack() }
        case .forward: if v.canGoForward { v.goForward() }
        }
        return true
    }

    // MARK: - navigation

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        fail(error)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { fail(error) }

    private func fail(_ error: Error) {
        pending = nil
        if (error as NSError).code == NSURLErrorCancelled { return }
        guard PageAuth.isUnreachable(error) else { return }
        showingOffline = true
        let name = page.name.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        webView?.loadHTMLString(
            """
            <html><body style="font: 15px 'Inter Tight', -apple-system, sans-serif; color: #1a1a1a; \
            margin: 0; display: flex; height: 100vh; align-items: center; justify-content: center">
            <p>\(name) needs the CHAI VPN \u{2014} connect and press Reload</p></body></html>
            """, baseURL: nil)
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        // The page loaded with the login typed: now it is worth keeping.
        if let p = pending {
            PageAuth.remember(user: p.user, password: p.password, for: p.space)
            pending = nil
        }
    }

    func webView(
        _ webView: WKWebView, didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping @MainActor @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        let space = challenge.protectionSpace
        if challenge.previousFailureCount > 0 {
            PageAuth.forget(for: space)
            pending = nil
        }
        switch PageAuth.decide(
            method: space.authenticationMethod, previousFailureCount: challenge.previousFailureCount,
            stored: PageAuth.stored(for: space))
        {
        case .useStored(let c): completionHandler(.useCredential, c)
        case .systemDefault: completionHandler(.performDefaultHandling, nil)
        case .prompt:
            ask(space: space, retry: challenge.previousFailureCount > 0) { [weak self] user, password, remember in
                guard let user else { return completionHandler(.cancelAuthenticationChallenge, nil) }
                if remember { self?.pending = (user, password, space) }
                completionHandler(.useCredential, URLCredential(user: user, password: password, persistence: .forSession))
            }
        }
    }

    /// The login sheet: user, password, "Remember in Keychain" (ticked).
    private func ask(space: URLProtectionSpace, retry: Bool, done: @escaping @MainActor (String?, String, Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = retry ? "Sign in to \(page.name) again" : "Sign in to \(page.name)"
        alert.informativeText = "\(space.host)" + (space.realm.map { " \u{2014} \($0)" } ?? "")
        let user = NSTextField(frame: NSRect(x: 0, y: 56, width: 260, height: 24))
        user.placeholderString = "User"
        let pass = NSSecureTextField(frame: NSRect(x: 0, y: 28, width: 260, height: 24))
        pass.placeholderString = "Password"
        let remember = NSButton(checkboxWithTitle: "Remember in Keychain", target: nil, action: nil)
        remember.state = .on
        remember.frame = NSRect(x: 0, y: 0, width: 260, height: 22)
        let box = NSView(frame: NSRect(x: 0, y: 0, width: 260, height: 84))
        [user, pass, remember].forEach(box.addSubview)
        alert.accessoryView = box
        alert.addButton(withTitle: "Sign In")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = user
        let finish: (NSApplication.ModalResponse) -> Void = { r in
            MainActor.assumeIsolated {
                if r == .alertFirstButtonReturn {
                    done(user.stringValue, pass.stringValue, remember.state == .on)
                } else {
                    done(nil, "", false)
                }
            }
        }
        if let w = window { alert.beginSheetModal(for: w, completionHandler: finish) } else { finish(alert.runModal()) }
    }
}

/// The Pages menu and the windows it opens (one per page, reused).
@MainActor
final class PagesController: NSObject, NSMenuDelegate {
    private var pages: Pages
    private var controllers: [String: PageWindowController] = [:]

    init(defaults: UserDefaults = .standard) {
        pages = Pages(defaults: defaults)
    }

    /// Whether any page window (and so any WKWebView) exists yet.
    var webViewsMade: Bool { controllers.values.contains { $0.webView != nil } }

    func menuItem() -> NSMenuItem {
        let m = NSMenu(title: "Pages")
        m.delegate = self
        let top = NSMenuItem(title: "Pages", action: nil, keyEquivalent: "")
        top.submenu = m
        return top
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let list = pages.all
        for p in list {
            let key = p == list.first(where: { $0.name == "Vitals" }) ? "v" : ""
            let item = NSMenuItem(title: p.name, action: #selector(open(_:)), keyEquivalent: key)
            if !key.isEmpty { item.keyEquivalentModifierMask = [.command, .shift] }
            item.target = self
            item.representedObject = p.name
            menu.addItem(item)
        }
        menu.addItem(.separator())
        let add = NSMenuItem(title: "Add Page\u{2026}", action: #selector(addPage(_:)), keyEquivalent: "")
        add.target = self
        menu.addItem(add)
        for p in list {
            let item = NSMenuItem(title: "Remove \(p.name)\u{2026}", action: #selector(removePage(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = p.name
            menu.addItem(item)
        }
    }

    @objc private func open(_ sender: NSMenuItem) {
        guard let name = sender.representedObject as? String, let p = pages.all.first(where: { $0.name == name })
        else { return }
        show(p)
    }

    func show(_ p: Page) {
        let c = controllers[p.name].flatMap { $0.page == p ? $0 : nil } ?? PageWindowController(page: p)
        controllers[p.name] = c
        c.show()
    }

    /// `--shot-page`: opens the page, lets it load (or show its login sheet), captures its window and quits.
    func shoot(name: String, to url: URL) {
        guard let p = pages.all.first(where: { $0.name == name }) else { return NSApp.terminate(nil) }
        show(p)
        DispatchQueue.main.asyncAfter(deadline: .now() + 8) { [self] in
            if let w = controllers[name]?.window {
                if let (png, _) = windowPNG(w) { try? png.write(to: url) }
                // A login sheet is a window of its own: its own file beside the page's.
                if let sheet = w.attachedSheet, let (png, _) = windowPNG(sheet) {
                    try? png.write(to: url.deletingPathExtension().appendingPathExtension("sheet.png"))
                }
            }
            exit(0)  // a sheet still up would hold terminate(_:) back
        }
    }

    @objc private func addPage(_ sender: Any?) {
        let alert = NSAlert()
        alert.messageText = "Add a page"
        alert.informativeText = "A name for the menu, and the address (http or https)."
        let name = NSTextField(frame: NSRect(x: 0, y: 30, width: 320, height: 24))
        name.placeholderString = "Name"
        let url = NSTextField(frame: NSRect(x: 0, y: 0, width: 320, height: 24))
        url.placeholderString = "https://host:port/"
        let box = NSView(frame: NSRect(x: 0, y: 0, width: 320, height: 56))
        [name, url].forEach(box.addSubview)
        alert.accessoryView = box
        alert.addButton(withTitle: "Add")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = name
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        if pages.add(name: name.stringValue, url: url.stringValue) == .invalid { NSSound.beep() }
    }

    @objc private func removePage(_ sender: NSMenuItem) {
        guard let name = sender.representedObject as? String else { return }
        let alert = NSAlert()
        alert.messageText = "Remove \(name) from Pages?"
        alert.informativeText = "Its window closes. Add it again any time."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        controllers.removeValue(forKey: name)?.window?.close()
        pages.remove(name: name)
    }
}
