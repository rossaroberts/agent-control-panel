// Basic-auth for a page (nginx answers 401). The decision is a pure function of the challenge and what the macOS
// keychain already holds (URLCredentialStorage, `.permanent`, keyed by host + port + realm); the sheet that asks for a
// login is PageWindow's. A password lives only in the keychain, never in the repo, UserDefaults or a log.

import Foundation

enum ChallengeDecision: Equatable {
    /// Answer with the keychain's credential.
    case useStored(URLCredential)
    /// Ask for a login (there is none, or the one kept was refused).
    case prompt
    /// Not a Basic challenge (a certificate, say): WebKit's own handling.
    case systemDefault
}

/// The part of URLCredentialStorage the pages use (the keychain, for `.permanent` credentials); a fake stands in in tests.
protocol CredentialStore {
    func defaultCredential(for space: URLProtectionSpace) -> URLCredential?
    func credentials(for space: URLProtectionSpace) -> [String: URLCredential]?
    func set(_ credential: URLCredential, for space: URLProtectionSpace)
    func setDefaultCredential(_ credential: URLCredential, for space: URLProtectionSpace)
    func remove(_ credential: URLCredential, for space: URLProtectionSpace)
}
extension URLCredentialStorage: CredentialStore {}

enum PageAuth {
    static func decide(method: String, previousFailureCount: Int, stored: URLCredential?) -> ChallengeDecision {
        guard method == NSURLAuthenticationMethodHTTPBasic else { return .systemDefault }
        if previousFailureCount == 0, let stored, stored.user != nil, stored.hasPassword { return .useStored(stored) }
        return .prompt
    }

    /// What the keychain holds for the challenge's protection space (host + port + realm).
    static func stored(for space: URLProtectionSpace, in storage: any CredentialStore = URLCredentialStorage.shared) -> URLCredential? {
        storage.defaultCredential(for: space) ?? storage.credentials(for: space)?.values.first
    }

    /// Forgets a credential the server refused, so it is asked for again rather than replayed.
    static func forget(for space: URLProtectionSpace, in storage: any CredentialStore = URLCredentialStorage.shared) {
        for (_, c) in storage.credentials(for: space) ?? [:] { storage.remove(c, for: space) }
        if let d = storage.defaultCredential(for: space) { storage.remove(d, for: space) }
    }

    /// Keeps a credential in the keychain for the space.
    static func remember(user: String, password: String, for space: URLProtectionSpace,
                         in storage: any CredentialStore = URLCredentialStorage.shared)
    {
        let c = URLCredential(user: user, password: password, persistence: .permanent)
        storage.set(c, for: space)
        storage.setDefaultCredential(c, for: space)
    }

    /// Whether a load error means the host could not be reached (VPN off).
    static func isUnreachable(_ error: Error) -> Bool {
        let e = error as NSError
        guard e.domain == NSURLErrorDomain else { return false }
        return [NSURLErrorCannotFindHost, NSURLErrorDNSLookupFailed, NSURLErrorNotConnectedToInternet,
                NSURLErrorCannotConnectToHost, NSURLErrorTimedOut, NSURLErrorNetworkConnectionLost].contains(e.code)
    }
}

extension PageAuth {
    /// Whether a challenge's origin (host and port) is the page's own: the sheet is only ever for the page's server.
    static func sameOrigin(host: String, port: Int, page: URL) -> Bool {
        guard let ph = page.host else { return false }
        let pp = page.port ?? (page.scheme?.lowercased() == "http" ? 80 : 443)
        let cp = port == 0 ? pp : port
        return host.caseInsensitiveCompare(ph) == .orderedSame && cp == pp
    }

    /// The plain in-window text for a failed load, nil for what is not worth one (a cancel, a refused navigation).
    static func message(for error: Error, page name: String) -> String? {
        let e = error as NSError
        if e.domain == NSURLErrorDomain && e.code == NSURLErrorCancelled { return nil }
        if e.domain == "WebKitErrorDomain" && e.code == 102 { return nil }  // interrupted by a policy decision
        if isUnreachable(error) { return "\(name) needs the CHAI VPN \u{2014} connect and press Reload" }
        return "\(name) could not be loaded (\(e.localizedDescription)). Press Reload to try again."
    }
}

/// What a navigation may do.
enum NavDecision: Equatable {
    case allow
    case cancel
    /// A new-window link to the page's own server: loaded in the same view.
    case loadHere(URL)
    /// A new-window link elsewhere: the default browser.
    case external(URL)
}

enum PageNavPolicy {
    /// Only http and https go anywhere (about:blank is the in-window message). `newWindow`: target=_blank and the like.
    static func decide(url: URL?, pageURL: URL?, newWindow: Bool) -> NavDecision {
        guard let url, let scheme = url.scheme?.lowercased() else { return .cancel }
        if scheme == "about" && url.absoluteString == "about:blank" { return newWindow ? .cancel : .allow }
        guard scheme == "http" || scheme == "https" else { return .cancel }
        guard newWindow else { return .allow }
        if let pageURL, let h = url.host,
            sameOrigin(host: h, port: url.port ?? (scheme == "http" ? 80 : 443), page: pageURL)
        {
            return .loadHere(url)
        }
        return .external(url)
    }
    private static func sameOrigin(host: String, port: Int, page: URL) -> Bool {
        PageAuth.sameOrigin(host: host, port: port, page: page)
    }
}

/// The 401s waiting on one sign-in sheet. Every reply is answered exactly once: with the credential, or cancelled
/// (when the sheet is cancelled, the window closes, or the queue goes away), so WebKit is never left waiting.
final class ChallengeQueue: @unchecked Sendable {
    typealias Reply = @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    private var waiting: [Reply] = []
    var isEmpty: Bool { waiting.isEmpty }
    var count: Int { waiting.count }

    func add(_ r: @escaping Reply) { waiting.append(r) }

    func resolve(_ c: URLCredential) {
        let all = waiting
        waiting = []
        all.forEach { $0(.useCredential, c) }
    }

    func cancelAll() {
        let all = waiting
        waiting = []
        all.forEach { $0(.cancelAuthenticationChallenge, nil) }
    }

    deinit { cancelAll() }
}
