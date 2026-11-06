// Which sessions a terminal opens on, as `public/fleet.html` decides it (`attachable`, `codexTerm`, lines 1050-1055):
// a live Claude Code background job (an 8-hex job id) attaches with `claude attach`; a Codex thread that is closed
// resumes with `codex resume`, but only when asked (it takes the thread up and carries its work on); nothing else has a
// terminal, and the pane says why. fleetd attaches only on its own machine: a session of another machine opens through
// that machine's fleetd (the caller passes its machine as `local`), and not while it cannot be reached.

public enum Attach: Equatable, Sendable {
    /// Opens when shown.
    case open(AttachTarget)
    /// Opens only when asked (Return in the pane): the words say what opening does.
    case ask(AttachTarget, String)
    /// No terminal, and why.
    case none(String)

    public static func of(_ s: Session, local: String?) -> Attach {
        if let local, s.key.machine != local { return .none("It runs on \(s.key.machine): open it there.") }
        if s.state == .unreachable {
            return .none("\(s.key.machine) cannot be reached now: its terminal opens here when it is back.")
        }
        switch s.engine {
        case .claude:
            guard let job = s.jobId, isJob(job) else {
                return .none("An interactive session: it is attached to the terminal it runs in.")
            }
            guard s.live else { return .none("Its background job is no longer running: nothing to attach to.") }
            return .open(.claude(profile: s.key.profile, job: job))
        case .codex:
            guard isThread(s.key.sessionId) else { return .none("Not a Codex thread Fleet can resume.") }
            guard let cwd = s.cwd, !cwd.isEmpty else { return .none("Its folder is not known, so it cannot resume here.") }
            guard !s.live else { return .none("It is open in Codex: answer it there.") }
            return .ask(
                .codex(profile: s.key.profile, sessionId: s.key.sessionId, cwd: cwd),
                "Return resumes this Codex thread here, and it carries on its work. Leaving it ends it.")
        case .muse:
            return .none("Nothing to attach to.")
        }
    }

    /// `claude attach` takes the roster's 8-hex job id.
    static func isJob(_ s: String) -> Bool {
        s.utf8.count == 8 && s.utf8.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x61 && $0 <= 0x66) }
    }

    /// A Codex thread id: a 36-character UUID, lower-case hex and dashes.
    static func isThread(_ s: String) -> Bool {
        s.utf8.count == 36 && s.utf8.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x61 && $0 <= 0x66) || $0 == 0x2d }
    }
}
