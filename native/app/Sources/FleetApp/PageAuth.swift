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

enum PageAuth {
    static func decide(method: String, previousFailureCount: Int, stored: URLCredential?) -> ChallengeDecision {
        guard method == NSURLAuthenticationMethodHTTPBasic else { return .systemDefault }
        if previousFailureCount == 0, let stored, stored.user != nil, stored.hasPassword { return .useStored(stored) }
        return .prompt
    }

    /// What the keychain holds for the challenge's protection space (host + port + realm).
    static func stored(for space: URLProtectionSpace, in storage: URLCredentialStorage = .shared) -> URLCredential? {
        storage.defaultCredential(for: space) ?? storage.credentials(for: space)?.values.first
    }

    /// Forgets a credential the server refused, so it is asked for again rather than replayed.
    static func forget(for space: URLProtectionSpace, in storage: URLCredentialStorage = .shared) {
        for (_, c) in storage.credentials(for: space) ?? [:] { storage.remove(c, for: space) }
        if let d = storage.defaultCredential(for: space) { storage.remove(d, for: space) }
    }

    /// Keeps a credential in the keychain for the space.
    static func remember(user: String, password: String, for space: URLProtectionSpace,
                         in storage: URLCredentialStorage = .shared)
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
