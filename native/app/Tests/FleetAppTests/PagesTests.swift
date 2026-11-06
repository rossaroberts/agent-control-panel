// The Pages list (defaults, round trip, validation), the basic-auth decision, and that no web view exists until a page
// is opened.

import Foundation
import Testing

@testable import FleetApp

private func scratch() -> UserDefaults {
    let name = "fleet.pages.test.\(UUID().uuidString)"
    let d = UserDefaults(suiteName: name)!
    d.removePersistentDomain(forName: name)
    return d
}

@Suite struct PagesTests {
    @Test func defaultsToVitals() {
        let p = Pages(defaults: scratch())
        #expect(p.all == [Page(name: "Vitals", url: "https://vitals.clintonhealthaccess.org:8780/")])
        #expect(p.all[0].address?.port == 8780)
    }

    @Test func roundTripsAddAndRemove() {
        let d = scratch()
        var p = Pages(defaults: d)
        #expect(p.add(name: " Docs ", url: "docs.example.org/x") == .added)
        #expect(Pages(defaults: d).all.map(\.name) == ["Vitals", "Docs"])
        #expect(Pages(defaults: d).all[1].url == "https://docs.example.org/x")
        #expect(p.add(name: "docs", url: "http://other.example.org") == .replaced)
        #expect(Pages(defaults: d).all.count == 2)
        p.remove(name: "Vitals")
        p.remove(name: "docs")
        #expect(Pages(defaults: d).all.isEmpty)  // emptied stays empty, not back to the default
    }

    @Test func refusesWhatIsNotAPage() {
        var p = Pages(defaults: scratch())
        #expect(p.add(name: "", url: "https://a.example") == .invalid)
        #expect(p.add(name: "X", url: "ftp://a.example") == .invalid)
        #expect(p.add(name: "X", url: "") == .invalid)
        #expect(p.all.count == 1)
    }

    @Test func noPasswordInDefaults() {
        let d = scratch()
        var p = Pages(defaults: d)
        p.add(name: "A", url: "https://a.example/")
        #expect(String(decoding: d.data(forKey: Pages.key) ?? Data(), as: UTF8.self).lowercased().contains("password") == false)
    }
}

@Suite struct PageAuthTests {
    let cred = URLCredential(user: "u", password: "p", persistence: .permanent)

    @Test func keychainHitIsUsed() {
        #expect(PageAuth.decide(method: NSURLAuthenticationMethodHTTPBasic, previousFailureCount: 0, stored: cred)
            == .useStored(cred))
    }

    @Test func noCredentialPrompts() {
        #expect(PageAuth.decide(method: NSURLAuthenticationMethodHTTPBasic, previousFailureCount: 0, stored: nil) == .prompt)
    }

    @Test func rejectedCredentialPromptsInsteadOfReplaying() {
        #expect(PageAuth.decide(method: NSURLAuthenticationMethodHTTPBasic, previousFailureCount: 1, stored: cred) == .prompt)
    }

    @Test func nonBasicIsDefaultHandling() {
        #expect(PageAuth.decide(method: NSURLAuthenticationMethodServerTrust, previousFailureCount: 0, stored: cred)
            == .systemDefault)
        #expect(PageAuth.decide(method: NSURLAuthenticationMethodNTLM, previousFailureCount: 0, stored: nil) == .systemDefault)
    }

    @Test func unreachableErrors() {
        #expect(PageAuth.isUnreachable(NSError(domain: NSURLErrorDomain, code: NSURLErrorCannotFindHost)))
        #expect(!PageAuth.isUnreachable(NSError(domain: NSURLErrorDomain, code: NSURLErrorCancelled)))
        #expect(!PageAuth.isUnreachable(NSError(domain: "x", code: NSURLErrorCannotFindHost)))
    }

    @MainActor @Test func noWebViewUntilOpened() {
        let c = PagesController(defaults: scratch())
        let menu = c.menuItem().submenu!
        // no menuNeedsUpdate: the items are there on a cold launch, before the menu was ever opened
        #expect(menu.items.first?.title == "Vitals")
        #expect(menu.items.first?.keyEquivalent == "v")
        #expect(menu.items.first?.keyEquivalentModifierMask == [.command, .shift])
        #expect(!c.webViewsMade)
    }
}

private final class MemoryStore: CredentialStore {
    var all: [URLProtectionSpace: [String: URLCredential]] = [:]
    var defaults: [URLProtectionSpace: URLCredential] = [:]
    func defaultCredential(for s: URLProtectionSpace) -> URLCredential? { defaults[s] }
    func credentials(for s: URLProtectionSpace) -> [String: URLCredential]? { all[s] }
    func set(_ c: URLCredential, for s: URLProtectionSpace) { all[s, default: [:]][c.user ?? ""] = c }
    func setDefaultCredential(_ c: URLCredential, for s: URLProtectionSpace) { defaults[s] = c }
    func remove(_ c: URLCredential, for s: URLProtectionSpace) { all[s]?[c.user ?? ""] = nil; if defaults[s] == c { defaults[s] = nil } }
}

@Suite struct PageFixRoundTests {
    @Test func userinfoIsRefusedAndNeverSaved() {
        let d = scratch()
        var p = Pages(defaults: d)
        #expect(Pages.parseURL("https://bob:hunter2@a.example/") == nil)
        #expect(Pages.parseURL("https://bob@a.example/") == nil)
        #expect(p.add(name: "Bad", url: "https://bob:hunter2@a.example/") == .invalid)
        let raw = String(decoding: d.data(forKey: Pages.key) ?? Data(), as: UTF8.self)
        #expect(!raw.contains("hunter2"))
        #expect(p.all.count == 1)
    }

    @MainActor @Test func vitalsShortcutMatchesAnyCaseOnColdLaunch() {
        let d = scratch()
        var p = Pages(defaults: d)
        p.all = [Page(name: "VITALS", url: "https://v.example/")]
        let menu = PagesController(defaults: d).menuItem().submenu!
        #expect(menu.items.first?.keyEquivalent == "v")
    }

    let page = URL(string: "https://vitals.example.org:8780/")!
    @Test func onlyHttpAndHttpsGoAnywhere() {
        for u in ["file:///etc/passwd", "javascript:alert(1)", "data:text/html,x", "ftp://a.example/", "x-apple.systempreferences:"] {
            #expect(PageNavPolicy.decide(url: URL(string: u), pageURL: page, newWindow: false) == .cancel, "\(u)")
        }
        #expect(PageNavPolicy.decide(url: nil, pageURL: page, newWindow: false) == .cancel)
        #expect(PageNavPolicy.decide(url: URL(string: "https://other.example/"), pageURL: page, newWindow: false) == .allow)
        #expect(PageNavPolicy.decide(url: URL(string: "about:blank"), pageURL: page, newWindow: false) == .allow)
    }

    @Test func newWindowLinksStayOrGoToTheBrowser() {
        let same = URL(string: "https://vitals.example.org:8780/x")!
        let other = URL(string: "https://elsewhere.example/")!
        let otherPort = URL(string: "https://vitals.example.org:9000/")!
        #expect(PageNavPolicy.decide(url: same, pageURL: page, newWindow: true) == .loadHere(same))
        #expect(PageNavPolicy.decide(url: other, pageURL: page, newWindow: true) == .external(other))
        #expect(PageNavPolicy.decide(url: otherPort, pageURL: page, newWindow: true) == .external(otherPort))
        #expect(PageNavPolicy.decide(url: URL(string: "file:///x"), pageURL: page, newWindow: true) == .cancel)
    }

    @Test func challengeOriginMustBeThePages() {
        #expect(PageAuth.sameOrigin(host: "Vitals.Example.org", port: 8780, page: page))
        #expect(!PageAuth.sameOrigin(host: "vitals.example.org", port: 8781, page: page))
        #expect(!PageAuth.sameOrigin(host: "evil.example", port: 8780, page: page))
        #expect(PageAuth.sameOrigin(host: "a.example", port: 0, page: URL(string: "https://a.example/")!))
        #expect(PageAuth.sameOrigin(host: "a.example", port: 443, page: URL(string: "https://a.example/")!))
        #expect(!PageAuth.sameOrigin(host: "a.example", port: 443, page: URL(string: "http://a.example/")!))
    }

    @Test func messagesForFailedLoads() {
        func err(_ c: Int) -> Error { NSError(domain: NSURLErrorDomain, code: c) }
        #expect(PageAuth.message(for: err(NSURLErrorCannotFindHost), page: "Vitals")
            == "Vitals needs the CHAI VPN \u{2014} connect and press Reload")
        #expect(PageAuth.message(for: err(NSURLErrorCancelled), page: "Vitals") == nil)
        #expect(PageAuth.message(for: NSError(domain: "WebKitErrorDomain", code: 102), page: "Vitals") == nil)
        #expect(PageAuth.message(for: err(NSURLErrorServerCertificateUntrusted), page: "Vitals")?.contains("could not be loaded") == true)
    }

    @Test func everyWaitingChallengeIsAnsweredOnce() {
        final class Got: @unchecked Sendable { var log: [String] = [] }
        let got = Got()
        func reply(_ n: String) -> ChallengeQueue.Reply { { d, c in got.log.append("\(n):\(d.rawValue):\(c?.user ?? "-")") } }
        let cred = URLCredential(user: "u", password: "p", persistence: .forSession)
        let q = ChallengeQueue()
        q.add(reply("a")); q.add(reply("b"))
        #expect(q.count == 2)
        q.resolve(cred)
        #expect(got.log == ["a:0:u", "b:0:u"])  // .useCredential
        q.resolve(cred)  // nothing left: no double answer
        #expect(got.log.count == 2)
        q.add(reply("c")); q.cancelAll()
        #expect(got.log.last == "c:2:-")  // .cancelAuthenticationChallenge
        var dropped: ChallengeQueue? = ChallengeQueue()
        dropped?.add(reply("d"))
        dropped = nil  // deinit answers it
        #expect(got.log.last == "d:2:-")
    }

    @Test func keychainStoredForgetRememberAndTheRejectLoop() {
        let storage = MemoryStore()
        let space = URLProtectionSpace(host: "vitals.example.org", port: 8780, protocol: "https", realm: "Box",
                                       authenticationMethod: NSURLAuthenticationMethodHTTPBasic)
        #expect(PageAuth.stored(for: space, in: storage) == nil)
        #expect(PageAuth.decide(method: space.authenticationMethod, previousFailureCount: 0, stored: PageAuth.stored(for: space, in: storage)) == .prompt)
        PageAuth.remember(user: "u", password: "p", for: space, in: storage)
        let c = PageAuth.stored(for: space, in: storage)
        #expect(c?.user == "u" && c?.password == "p")
        #expect(PageAuth.decide(method: space.authenticationMethod, previousFailureCount: 0, stored: c) == .useStored(c!))
        // the server refuses it: it prompts, the credential is forgotten, and the next challenge starts with a prompt
        #expect(PageAuth.decide(method: space.authenticationMethod, previousFailureCount: 1, stored: c) == .prompt)
        PageAuth.forget(for: space, in: storage)
        #expect(PageAuth.stored(for: space, in: storage) == nil)
        #expect(PageAuth.decide(method: space.authenticationMethod, previousFailureCount: 0, stored: PageAuth.stored(for: space, in: storage)) == .prompt)
    }
}
