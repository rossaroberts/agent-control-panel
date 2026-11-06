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
        (menu.delegate as? PagesController)?.menuNeedsUpdate(menu)
        #expect(menu.items.first?.title == "Vitals")
        #expect(menu.items.first?.keyEquivalent == "v")
        #expect(menu.items.first?.keyEquivalentModifierMask == [.command, .shift])
        #expect(!c.webViewsMade)
    }
}
