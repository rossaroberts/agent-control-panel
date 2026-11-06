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
final class PageWindowController: NSObject, WKNavigationDelegate, WKUIDelegate, NSWindowDelegate {
    let page: Page
    private(set) var window: PageWindow?
    private(set) var webView: WKWebView?
    private var showingOffline = false
    private var backButton: NSButton?, forwardButton: NSButton?
    /// A login just typed with "Remember" ticked: kept in the keychain once the page loads (a refused one is dropped).
    private var pending: (user: String, password: String, space: URLProtectionSpace)?
    /// The 401s waiting on the one sheet; answered (cancelled) if the window closes, whatever else happens.
    private let waiting = ChallengeQueue()
    private var sheetOpen = false

    init(page: Page) {
        self.page = page
    }

    func show() {
        if window == nil { build() }
        window?.makeKeyAndOrderFront(nil)
        NSApp.activate()
    }

    /// Closes the window (and with it the web view and any sheet).
    func close() { window?.close() }

    private func build() {
        let w = PageWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1100, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = page.name
        w.isReleasedWhenClosed = false
        w.delegate = self
        w.center()
        w.setFrameAutosaveName("FleetPage-\(page.name)")  // after center(): a saved frame wins
        let v = WKWebView(frame: w.contentLayoutRect, configuration: WKWebViewConfiguration())
        v.autoresizingMask = [.width, .height]
        v.navigationDelegate = self
        v.uiDelegate = self
        v.allowsBackForwardNavigationGestures = true
        w.contentView = v
        w.onKey = { [weak self] a in self?.handle(a) ?? false }
        w.addTitlebarAccessoryViewController(buttons())
        window = w
        webView = v
        load()
    }

    /// Back, Forward and Reload, visible in the title bar (the keys work too).
    private func buttons() -> NSTitlebarAccessoryViewController {
        func button(_ symbol: String, _ tip: String, _ action: Selector) -> NSButton {
            let b = NSButton(image: NSImage(systemSymbolName: symbol, accessibilityDescription: tip) ?? NSImage(),
                             target: self, action: action)
            b.bezelStyle = .texturedRounded
            b.toolTip = tip
            return b
        }
        let back = button("chevron.left", "Back (\u{2318}\u{2325}\u{2190})", #selector(goBack))
        let fwd = button("chevron.right", "Forward (\u{2318}\u{2325}\u{2192})", #selector(goForward))
        let reload = button("arrow.clockwise", "Reload (\u{2318}R)", #selector(reloadPage))
        back.isEnabled = false
        fwd.isEnabled = false
        backButton = back
        forwardButton = fwd
        let stack = NSStackView(views: [back, fwd, reload])
        stack.spacing = 4
        stack.edgeInsets = NSEdgeInsets(top: 0, left: 8, bottom: 0, right: 8)
        let vc = NSTitlebarAccessoryViewController()
        vc.view = stack
        vc.layoutAttribute = .trailing
        return vc
    }

    @objc private func goBack() { _ = handle(.back) }
    @objc private func goForward() { _ = handle(.forward) }
    @objc private func reloadPage() { _ = handle(.reload) }

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

    private func syncButtons() {
        backButton?.isEnabled = webView?.canGoBack ?? false
        forwardButton?.isEnabled = webView?.canGoForward ?? false
    }

    // MARK: - closing

    func windowWillClose(_ notification: Notification) {
        if let w = window, let sheet = w.attachedSheet { w.endSheet(sheet) }
        waiting.cancelAll()  // no 401 is left unanswered
        pending = nil
        webView?.stopLoading()
        webView?.navigationDelegate = nil
        webView?.uiDelegate = nil
        // Released once the close has finished; the next open builds a fresh view (the frame is remembered).
        DispatchQueue.main.async { [self] in
            window?.contentView = nil
            window?.delegate = nil
            webView = nil
            window = nil
        }
    }

    // MARK: - navigation

    /// Only http and https go anywhere; anything else is cancelled.
    func webView(
        _ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
    ) {
        route(PageNavPolicy.decide(url: action.request.url, pageURL: page.address, newWindow: action.targetFrame == nil),
              decisionHandler)
    }

    /// target=_blank and window.open: never a second web view; same server loads here, another goes to the browser.
    func webView(
        _ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
        for action: WKNavigationAction, windowFeatures: WKWindowFeatures
    ) -> WKWebView? {
        route(PageNavPolicy.decide(url: action.request.url, pageURL: page.address, newWindow: true)) { _ in }
        return nil
    }

    private func route(_ d: NavDecision, _ decide: @MainActor (WKNavigationActionPolicy) -> Void) {
        switch d {
        case .allow: decide(.allow)
        case .cancel: decide(.cancel)
        case .loadHere(let u):
            decide(.cancel)
            webView?.load(URLRequest(url: u))
        case .external(let u):
            decide(.cancel)
            NSWorkspace.shared.open(u)
        }
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        fail(error)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { fail(error) }

    private func fail(_ error: Error) {
        pending = nil
        guard let text = PageAuth.message(for: error, page: page.name) else { return }
        showingOffline = true
        let safe = text.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        webView?.loadHTMLString(
            """
            <html><body style="font: 15px 'Inter Tight', -apple-system, sans-serif; color: #1a1a1a; \
            margin: 0; display: flex; height: 100vh; align-items: center; justify-content: center">
            <p>\(safe)</p></body></html>
            """, baseURL: nil)
        syncButtons()
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        // The page loaded with the login typed: now it is worth keeping.
        if let p = pending {
            PageAuth.remember(user: p.user, password: p.password, for: p.space)
            pending = nil
        }
        syncButtons()
    }

    func webView(
        _ webView: WKWebView, didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping @MainActor @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        let space = challenge.protectionSpace
        // A login is only ever asked for, or sent, on behalf of the page's own server.
        let own = page.address.map { PageAuth.sameOrigin(host: space.host, port: space.port, page: $0) } ?? false
        guard own else { return completionHandler(.cancelAuthenticationChallenge, nil) }
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
            waiting.add { d, c in MainActor.assumeIsolated { completionHandler(d, c) } }
            guard !sheetOpen else { return }  // one sheet answers every 401 waiting on it
            sheetOpen = true
            ask(space: space, retry: challenge.previousFailureCount > 0) { [weak self] user, password, remember in
                guard let self else { return }
                self.sheetOpen = false
                guard let user else { return self.waiting.cancelAll() }
                if remember { self.pending = (user, password, space) }
                self.waiting.resolve(URLCredential(user: user, password: password, persistence: .forSession))
            }
        }
    }

    /// The login sheet: which server asks (host:port), user, password, "Remember in Keychain" (ticked).
    private func ask(space: URLProtectionSpace, retry: Bool, done: @escaping @MainActor (String?, String, Bool) -> Void) {
        guard let w = window else { return done(nil, "", false) }
        let alert = NSAlert()
        alert.messageText = retry ? "Sign in to \(page.name) again" : "Sign in to \(page.name)"
        alert.informativeText = "\(space.host):\(space.port)" + (space.realm.map { " \u{2014} \($0)" } ?? "")
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
        alert.beginSheetModal(for: w) { r in
            MainActor.assumeIsolated {
                if r == .alertFirstButtonReturn {
                    done(user.stringValue, pass.stringValue, remember.state == .on)
                } else {
                    done(nil, "", false)
                }
            }
        }
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
        menuNeedsUpdate(m)  // filled now, not only when first opened: ⌘⇧V works on a cold launch
        return top
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let list = pages.all
        for p in list {
            let key = p == list.first(where: { $0.name.caseInsensitiveCompare("Vitals") == .orderedSame }) ? "v" : ""
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
        if let old = controllers[p.name], old.page != p {  // its address changed: the old window goes
            old.close()
            controllers[p.name] = nil
        }
        let c = controllers[p.name] ?? PageWindowController(page: p)
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
        switch pages.add(name: name.stringValue, url: url.stringValue) {
        case .invalid: NSSound.beep()
        case .replaced:  // same name, maybe a new address: the old window (and its web view) goes
            for k in controllers.keys where k.caseInsensitiveCompare(name.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)) == .orderedSame {
                controllers.removeValue(forKey: k)?.close()
            }
        case .added: break
        }
    }

    @objc private func removePage(_ sender: NSMenuItem) {
        guard let name = sender.representedObject as? String else { return }
        let alert = NSAlert()
        alert.messageText = "Remove \(name) from Pages?"
        alert.informativeText = "Its window closes. Add it again any time."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        controllers.removeValue(forKey: name)?.close()
        pages.remove(name: name)
    }
}
