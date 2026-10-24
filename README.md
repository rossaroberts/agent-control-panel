# Agent panel

A local browser page that shows every Claude Code session on this machine and lets you answer
them without switching terminals.

## Get started

You need a Mac (macOS 14 or newer) with Node 20 or newer. Open https://universe.cashegg.com/download, sign
in with GitHub, and paste the one line it shows into Terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/IcyDingo/agent-control-panel/main/hub/public/install.sh | sh
```

It installs a small background service that counts your agents' work and keeps your galaxy up to date, and
links this Mac to your account. The script runs from GitHub, not from the hub, and installs only a release that is on
`main` and signed by the maintainer's key. Your galaxy page is the whole interface; the Mac app below is optional.

Updates are daily, only to a release signed by the project's key on the maintainer's Mac, never to an older one, and a version that cannot start is rolled back; to turn them off:
`printf '{ "auto": false }\n' > "$HOME/Library/Application Support/Agent Panel/worker/update.json"`

### The session board (optional)

The board is the Mac app's own interface, and the rest of this README describes it. You need Claude Code
2.1.303 or newer as well.

```bash
git clone https://github.com/IcyDingo/agent-control-panel.git
cd agent-control-panel
./install.sh
```

`install.sh` checks for Node 20+ and Apple's Command Line Tools (`xcode-select --install`; Xcode
itself isn't needed), tells you what to install if something is missing, and builds **Agent
Panel.app** into `~/Applications`. Then:

1. Open **Agent Panel**. macOS may ask whether it may use your Documents folder: the panel reads the
   repositories your sessions work in.
2. If you use more than one Claude Code profile, pick yours under **Profile**, top right.
3. Click **Connect this profile**. It adds the panel's hooks to that profile's `settings.json` (and
   keeps a copy of the old file). Sessions you start after that show up on the board.

`install.sh` also installs a background service, so World keeps counting your work when the app isn't open;
`./install.sh --no-daemon` skips it. If the worker above is already installed, this service takes over
from it: the background service then runs from your clone, and says so while it installs.

Keep the folder where you cloned it: the service and the app run the panel from there. To update, `git pull`
and run `./install.sh` again: it rebuilds the app and restarts the service on the new code. Signed in to the hub,
the board also says when the hub runs a newer signed release than your clone (it asks a minute after start, then
every 6 hours) and offers **Update**: it fetches `main` and fast-forwards to that release, rebuilds the app where it
is, and the panel restarts on the new code. A release no known key signed is never offered. It won't touch a clone on
another branch or with uncommitted edits, and says so; then update it yourself as above. To use it without the app,
see [Run it](#run-it). To take it out again, see [Uninstall](#uninstall).

The panel's own work (sessions, decisions, World) stays on your Mac whether or not you sign in: its server
listens on 127.0.0.1 only, and the page fetches nothing but its typeface (from Google Fonts). The Mac sends
nothing to the hub until it is signed in: by the one line run from the Download page while signed in, by
`node server.mjs login`, or from the panel's Profile box.

**The board** stacks four queues in a rail on the left: **Needs you**, **Working**, **Idle** and
**Finished**. Needs you is always open. Click any other queue's header to fold it to one line, which
keeps its count and the name of its newest session; the panel remembers what you folded, and starts
with Idle and Finished folded. Drag the rail's right edge to widen it (double-click for the default).
Each session is a tile with its name, how many subagents it's running, its age, and one line saying
what it's doing or what it last said. Click a tile to open its panel in the rest of the window. The
panel reads like a chat: its name and what you can do on top, the conversation in the middle, and
what it's asking, what it's doing now and the box to write in pinned along the bottom. The chat keeps
to reading width on a big screen. **Details** folds away its folder and branch, and **Expand** gives it
the whole window (**Collapse** brings the board back). Its subagents, each titled with the task its
parent gave it, get a column on the right when the panel is wide (its tab folds it to a strip) and sit
under the conversation when it isn't. When the open session goes away, the top of Needs you takes its place;
close it yourself and the space offers that one instead. On a narrow window the rail takes the width
and an open session covers it.

Each tile also shows the session's **branch**: a branch mark when it works in a repository's main
checkout, a folder mark when it is in a worktree. The panel says which worktree of which repository, and
names the other live sessions in the same checkout, since they share its branch and files (one that
switches the branch switches it for all of them). Where a session works comes from its transcript, so it
follows a session that moves into a worktree after it starts, which `claude agents` doesn't (it keeps the
folder the session started in). When a session edits files in other checkouts, its panel lists those too
("Also edits files in agent-control-panel (main)"). Branches are read from the files under `.git`, never
by running git.

**Space view** shows the same sessions full screen, as satellites orbiting you, each one a body in its
state's colour between two solar wings, flying along its orbit. Its orbit is its state: the inner
orbit needs you, then working, idle and finished. Background sessions carry a dish, subagents are
small satellites circling their session's, and a request shows as an amber tether to you. Click a satellite
for the same panel, click the centre to jump to the next request, and press Esc to go back.
**Earth**, bottom left, launches a new agent: a rocket lifts off and flies to a free slot on its
orbit, and the satellite appears where the rocket arrives and opens its wings. Its panel then opens,
unless you are typing in the open one. With reduced motion there is no flight: the satellite simply
appears.

**World**, next to Space view, is a world your agents' work builds (see World, below). Once it has
started, its button shows the credit you have to spend; hover it for today's lines, commits, active time and
credit earned.

When a session or subagent asks for something, the request appears in its panel:

- **Permission:** allow it, edit the command first, allow and remember, or deny with a reason.
- **Question** (`AskUserQuestion`): pick an option per question, or type your own answer.
- **Plan** (`ExitPlanMode`): read the plan, then approve it (in auto mode, or asking before edits)
  or send it back with what to change.

**New agent** launches a background session (`claude --bg`) with a prompt, folder, name, agent type,
model and permission mode (rows of buttons), and effort (a slider; **Default** leaves it to your profile's
own default). The folder can be any folder: **Choose…** opens the Mac's folder
dialog, and typing a path suggests the folders under it. A folder Claude Code hasn't been trusted in
yet needs `claude` run there once first, to accept its trust prompt.

**Its own worktree** launches it with `claude --worktree`: Claude Code makes
`.claude/worktrees/<name>` on a new branch `worktree-<name>`, starting from the remote's default
branch (`origin/main`, say), or from the current commit when the repo sets
`worktree.baseRef: "head"`. The name follows the agent's name unless you type one; left blank,
Claude picks one. The launcher says where it will land before you launch. The switch is off for a
folder outside git. For the first few seconds the session is still in the folder you picked, while
Claude Code fetches and makes the worktree. Removing the session later removes its worktree too,
unless the worktree holds work.

The panel is dark unless you pick otherwise: the moon button, top right, offers **Dark**, **Light** or **Match
the Mac**, remembered in that browser (or the app).

It fills the gaps in `claude agents` (agent view): agent view can't answer a permission prompt
without attaching, it doesn't list sessions in other terminals until you `/bg` them, and it gives
subagents no rows of their own.

## Run it

```bash
node server.mjs            # or: npm start
open http://127.0.0.1:4747
```

Node 20+, and `python3` (it comes with macOS's command-line tools) for the helpers that work a background
session's screen: switching its mode, answering a question it no longer holds, running a `/command`. Nothing to
install. `ACP_PORT` changes the port; if you change it, change the hook URLs too.

Installed with `./install.sh`, the panel also runs as a launchd service (`local.agent-panel`, or
`local.agent-panel.<port>` on another port): `launchctl print gui/$UID/local.agent-panel` shows it,
`launchctl kickstart -k gui/$UID/local.agent-panel` restarts it, and its log is
`~/Library/Logs/Agent Panel/daemon-<port>.log`. `node server.mjs daemon install|remove|status` manages it.

## Fleet

`http://127.0.0.1:4747/` is Fleet: every agent on this Mac and on the machines you list, on one screen, so you can see
what each one is working on, talk to it, and decide where the next one goes. The board is at `/board`.

- **Accounts**, one line each and across machines: every Claude Code profile (`~/.claude`, `~/.claude-<name>`), every
  Codex home (`~/.codex`), and Muse where its CLI is signed in. Each shows its 5-hour and weekly limits, a cap and when it
  lifts, and its sessions by state; the one with the most room is first. Click one to launch with it.
- **Machines**, one line each: dev servers, database stacks, containers, load, memory and disk.
- **Launch**: write what it should do, pick the account, machine, folder (it offers the folders that account works in
  there, the latest first), model and effort, and ⌘↵. A folder the account hasn't trusted is refused with **trust it**:
  Claude Code opens in that folder in the pane with its own trust prompt; approve it there, close the pane, Fire again. `claude --bg` for a
  Claude account, `codex exec --sandbox workspace-write` for Codex, `muse exec` for Muse, or a fleet dispatch
  (`fleet-workers --dispatch`) on a machine with the fleet kit.
- **Needs you**, above everything: every session waiting on a question or permission, then every one stopped at a limit,
  the longest wait first, each with what it waits on under it. The tab's title carries the count.
- **Sessions**, grouped by epic (`epics` in `fleet.json`), then by the groups you make: each under its own name (the
  one it has in Claude Code: the app's, claude.ai's, `/rename`'s, or the one you give it in Fleet's launcher or pane);
  only one whose Claude Code name is a placeholder (`chai-expense-tracker-54`) gets a title Sonnet writes from what it
  was asked last, said last and did last. Sonnet also gives each a stage (planning, building, testing, promoting,
  deploying) and an epic. Its machine,
  account, effort, how full its context is, and about how much of its account's weekly limit it used. Every session is
  listed: open ones first, then one "N closed" line that opens them. An epic's closed sessions archive in one go.
- **Click a session** and it opens on the right, full height, as a terminal window: one line on top (its name, which is
  yours to change; ⋯ for the rest), then its screen. Drag the pane's edge to widen it; ⌘\\ or »
  folds it to a strip. Behind ⋯: what it is, its group (pick one, or type a new one; empty puts the written one back), one
  control that closes an open session or archives a closed one, **TEST** (the epic's TEST site in a window beside the
  panel), its claude.ai page and its attach command.
- **A background session opens as its terminal**: its own Claude Code screen, attached where it runs (`claude attach`,
  over `ssh -tt` on another machine). What you type reaches it as yours, `/model`, `/effort` and its permission prompts
  included. The last four you opened stay attached out of sight, so going back to one is instant (about 20 ms), and
  hovering one attaches ahead of the click. Where a key takes long to come back (redbat is a quarter of a second away),
  what you type shows at once over the cursor, an overlay only, until the real screen catches up. **/exit** ends the
  session and closes the pane. xterm draws on the GPU (WebGL) where it can.
- **An Astra (Codex) session** shows its conversation; one that is closed and finished its last turn has **terminal**,
  which opens it in Codex's own screen here (`codex resume <id>` in its folder, every permission allowed). Not a live one
  (open in the Codex app) or one that ended mid-turn: resuming takes the thread over and carries its work on.
- **A session running inside tmux** (anywhere, any account) opens as its tmux pane: `tmux -L <server> attach -t <pane>`,
  its real screen, nothing moved or restarted; /exit there ends Claude Code as it would in that window.
- Everything Fleet starts (a launch, **move here**, **continue here**) runs with every permission allowed:
  `--dangerously-skip-permissions`, and a project's MCP servers enabled without asking (for that session only); Codex
  with `--dangerously-bypass-approvals-and-sandbox`, Muse with `--yolo`.
- **A session that isn't a terminal** shows its conversation the way its terminal prints it: every step, nothing folded.
  A desktop-app session can't be typed into from here: **continue here** (a second click confirms) continues it as a copy
  in the background (`claude --bg --resume <id> --fork-session`), with its terminal, the app's own session untouched.
- **move here** (a second click confirms) takes an idle session, or one stopped at a limit, from a terminal or claude.ai
  into the background, conversation kept (`claude --bg --resume <id>`, same folder, model and permission mode), so it
  opens as a terminal too. Never one that works or waits on you, never by itself, and not a desktop-app session. Its old
  claude.ai page is done with: a message there starts a second process on the same conversation. It gets a new one.

How a message gets in, for a session that isn't a terminal here. A background job gets it as yours, the way agent view's
reply does (`claude --resume <id> -- <text>`); one still on an older Claude Code than the one installed says to type in its
terminal (restarting it is never a message's call). A Remote Control session (a bridge's, a desktop-app one with Remote
Control on, or any started with `"remoteControlAtStartup": true`) gets it through its claude.ai session,
`claude -p --cloud session_<id> <text>`: Claude Code hands that over as another session's note, so the session acts on it
but won't take `/model`, `/effort` or a yes it needs from you that way. A terminal session without Remote Control takes
typing only where it runs, or here after **move here**.

The terminal is a pseudo-terminal on this Mac: a few lines of Python (`/usr/bin/python3`, no native module to build) relay
its bytes, streamed to the page as one long response, and the page's keys come back as small requests. xterm.js draws it,
kept in `public/vendor` (MIT) so the page needs no network for it.

Where the numbers come from. Claude Code shows an account's limits only to its status line, so `fleet/statusline.mjs`
keeps the newest one per session in `<profile>/fleet-status/` (set it as each profile's `statusLine`; it prints
`ctx 20% · 5h 22% · 7d 2%`). A session with no screen (a Remote Control bridge's) has no status line: an account whose
sessions are all like that shows `·` until one runs as a terminal. Without a reading, an account's card shows the last
limit stop its sessions met. Two accounts reporting the same limits to the second get a red `?`: one profile is signed
in as the other's account (set `logins`, below). Codex writes
its limits into each session's log. A session's weekly share is the account's weekly figure split by each session's
tokens this week, weighted as usage is priced; transcripts are read once from the week's start and then only their new
lines (`~/.fleet-panel/usage.json`).

Other machines, names and epics go in `fleet.json` beside `prefs.json`:

```json
{ "self": "Mac",
  "machines": [ { "name": "server", "ssh": "my-server", "user": "agents" } ],
  "names": { "claude:me@example.com": "personal", "codex:me@example.com": "codex" },
  "epics": [ { "name": "Web app", "about": "the web app: features, CI, deploys", "test": "https://test.example.com" } ],
  "logins": { "Mac:default": "other@example.com" } }
```

`ssh` is a host or `~/.ssh/config` alias that logs in without a password; `user` is who the agents run as there (`sudo
-u`). Nothing is installed on it, and it needs Node 20+. The panel keeps one connection to it open (ssh's ControlMaster,
socket in `~/.fleet-panel/`) and, on it, one worker: `node` with `fleet/machine.mjs` sent once, answering a request a line,
so a read costs a round trip rather than a login. Terminals open as channels on the same connection. A read that fails
keeps the machine's last good one on the page, marked as such. `names` labels an account (keyed `engine:email`), `self`
names this Mac, each epic's `about` is what Sonnet files sessions by, and `logins` says which account a profile's login
really is where its config file says another (`machine:profile`). Titles are one Sonnet call for every session whose ask changed, and every ten minutes for one that works (no tools,
hooks, MCP servers or saved session), kept in `fleet-titles.json`; your names and groups in `fleet-labels.json`; archived sessions in
`fleet-archived.json`.

## The Mac app

`app/build.sh` builds **Agent Panel.app** in `~/Applications`: the panel in a window of its own, with
its own icon. Open it like any app, or keep it in the Dock.

- Opening it uses the background service's server, or any panel server already running, or starts one of its own
  (its log is `~/Library/Logs/Agent Panel/server-4747.log`). Quitting stops a server the app started and leaves
  one it didn't alone.
- If the server it was using goes away, it starts its own after a few seconds. One it started that
  stops after running a while is started again.
- The Dock icon counts the requests waiting on you, and bounces when a new one arrives while you're
  in another app. Closing the window keeps the panel connected: the count stays live and requests are
  still held. Click the Dock icon to bring the window back; ⌘Q quits.
- **Turn on desktop alerts** gives you the Mac's own notifications. Clicking one opens that session.
- The title bar follows the panel's theme.
- **File › New Agent** (⌘T) opens the launcher, and the **New agent** button shows the shortcut. In a
  browser ⌘T opens a new tab, so there the button shows none.
- Links to other sites open in your browser. ⌘R reloads, and starts a server if none answers;
  ⌘= / ⌘- / ⌘0 zoom.

An app opened from the Dock gets no shell environment, so the build bakes in this folder's path,
node's path and your shell's `PATH`. Build it again when one of those moves (an nvm upgrade, say).
`ACP_PORT=<port> app/build.sh` builds a separate app for a panel on another port.

The first time it starts the server, macOS asks whether Agent Panel may use your Documents folder: the
panel lives there, and it reads the sessions' repositories. The app is signed for this Mac only, so a
rebuilt app may ask again.

## Profiles

Each Claude Code profile is a config folder: `~/.claude` for plain `claude`, `~/.claude-<name>` for
one started with `CLAUDE_CONFIG_DIR=~/.claude-<name>`. The panel shows one profile at a time.
**Profile** in the header lists them by folder name (`claude`, `claude-work`), with each one's
account and plan from `claude auth status`, and switches between them.

- Switching swaps the board to that profile's sessions, and new agents launch in it.
- A request held for the old profile's sessions stays held, out of sight, and shows again when that
  profile is back; a permission request from a profile that isn't on the board is held the same way.
  An unsent message is dropped. When there is one, it asks first.
- The choice is remembered, in `~/Library/Application Support/Agent Panel/prefs.json` (`ACP_PREFS`
  names another file, so a second panel on another port keeps a choice of its own). Without one,
  the panel starts in the profile it was started under (`CLAUDE_CONFIG_DIR`), else the first one whose
  hooks point at it, else plain `claude`'s.

A profile's sessions report here only once its settings have the hooks. When a profile doesn't have
them, the panel shows **Connect this profile**. Clicking it adds the entries from
`hooks.example.json` that the profile's `settings.json` lacks. It changes nothing else in the file,
and keeps a dated copy of the old file beside it. Requests from a profile the panel isn't showing are
left to their terminals.

## Wire the hooks (once)

Click **Connect this profile** in the panel, or merge `hooks.example.json` into your user settings
yourself (`~/.claude/settings.json`, or `$CLAUDE_CONFIG_DIR/settings.json`). Sessions started after
that report to the panel; one that
was already running keeps asking in its terminal until you restart it. To try it
without touching your settings, run a single session with the hooks:
`claude --settings hooks.example.json`.

## How decisions work

- A permission request appears here **and** in the session's terminal at the same time.
  Whichever answers first wins.
- The panel holds a request only while a panel tab is open and **Answer prompts here** is on.
  Otherwise it answers nothing and the terminal decides as usual. A reload (yours, or the app's when
  its health check is slow) isn't a closed tab: held requests wait 15 seconds for a page to come back.
- If the server isn't running, the hooks fail silently and Claude Code behaves as if they weren't there.
- **Allow edited command** sends your edit as the tool's input. **Edit full input** does the same
  for any tool, as JSON.
- **Allow and remember** saves the rules Claude Code itself suggested for that request (the same
  rules as the terminal's "don't ask again" option), never rules typed in the browser.
- A question is answered by sending its answers back as the tool's input. A plan is approved by
  sending the plan back as input, plus the mode to carry on in: a bare allow leaves the terminal's
  plan dialog open. Sending a plan back is a deny, with your feedback as the message.
- If the server restarts, an open tab reloads itself once the server is back: each start mints a new token.

When the terminal answers first, Claude Code leaves the panel's request open. The panel retires
the card when the tool's `PostToolUse` arrives, when Claude Code closes the request (a cancel), or,
for main-agent requests, when the session list shows the session is no longer waiting.

## A question or plan it no longer holds

A held request goes back to the session after an hour, or when the panel server restarts. For a terminal session that
is fine: its dialog is on your screen. A background session has no screen of yours, so the panel still shows its open
**question** (`AskUserQuestion`), read from its transcript, with the same card. **Send answers** puts them in on its
own screen the way you would: `answer-question.py` attaches in a pseudo-terminal, checks that question is what it
shows, presses each answer's number (or "Type something", your words, Enter) and submits. The panel then checks
that its conversation got exactly the answers you picked.

An open **plan** (`ExitPlanMode`) gets its usual card too. **Approve** and **Send back** go in on its screen through
`answer-plan.py`: it finds the choice by its words (they vary: "Yes, and use auto mode", "Yes, manually approve
edits", "Tell Claude what to change", or "Yes, and switch to default" without a plan file), presses it only while
`claude agents` says the session waits on a permission prompt and its transcript's open call is that plan, and types
your feedback after "Tell Claude what to change". The panel then checks its conversation says approved, or carries
your words.

- Questions and plans only. A permission prompt it no longer holds is shown, with `claude attach <id>` to answer it
  there: pressing Yes on a permission prompt from outside would go around Claude Code's permission system. A plan's
  choices are the ones its held card offers anyway.
- A question that takes several picks at once isn't pressed from here either.
- The question has to be in the transcript, and Claude Code writes it there before a `PreToolUse` hook runs, so this
  works for the sessions that report to the panel.

## Messaging a session

Every session's panel has a **Chat**: the conversation so far, read from its transcript, with a message
box with the session's mode and **Send** along its bottom. Each of your messages shows with the reply
Claude ended on (not its running commentary between tool calls); **Show earlier** reads further back,
and a compacted stretch shows as a note. It opens at the newest message and keeps it in view as more
arrive, unless you scrolled up to read. When its last reply ends with a numbered or lettered list,
each option shows as a chip that fills the box; add to it if you like.

**Enter** sends at once: the message leaves the box and shows in the chat as sending. If it doesn't
go through, it goes back into the box (ahead of anything you typed since) and the panel says why.
Shift+Enter starts a new line.
How a message gets in depends on the kind of session, because only Claude Code's own ways in count
as you: text a hook hands back reaches the model as hook output, and it rightly won't take
instructions from that.

**A background session** gets it straight away, working or not, the way agent view's reply does:
`claude --resume <session id> -- <message>` (without `--bg`) hands it to the running job as your
next message, then attaches. The panel runs that under `script` for a terminal, waits until the
message is in the transcript, and closes the client. The session isn't stopped or restarted. While
it works, the message arrives at its next step as "The user sent a new message while you were
working", exactly as if you'd typed it. A `/command` goes in another way (see Slash commands), and a
message that starts with `!` (a shell command) isn't sent from here.

**A background session that has stopped** (ended, finished and closed, or blocked) but is still listed
wakes up with your message: `claude --bg --resume <session id> -- <message>`, with no other flags,
starts the same job again under the same id, with the options it started with (name, model, effort,
mode) and in the same folder or worktree, unpushed work included. Nothing is removed first.

**A terminal session** has no way in from outside its terminal. So:

- **While it works**, the panel holds your message (**Send when it is done** asks twice). When its
  turn is over, with no subagent running and nothing waiting on you, the panel closes it and carries
  on as below. You can take a held message back, or type in its terminal instead.
- **At its prompt, or closed**, a reply restarts the conversation with your words as a real prompt:
  the panel sends SIGTERM (Claude closes cleanly), then
  `claude --bg --resume <session id> --name <name> -- <your reply>` carries the same conversation on
  in the background with the model, effort and mode it last used, or the ones you pick beside **Send**.
  `claude attach` opens it again.

Also:

- A session that hasn't saved a conversation yet (a terminal opened but never prompted) is
  refused before anything is closed: there would be nothing to resume.
- `claude agents` reports a session as busy for as long as a background task it started (a dev
  server, a watch) runs, even when its turn is over and it waits at its prompt. The panel reads the
  end of its transcript to tell the two apart, so such a session shows as idle. Closing it stops
  that task, and the panel says so before you confirm.
- What a session last said comes from its `Stop` hook, or from its transcript for a session that
  finished its turn before the panel started.
- A background session that is no longer listed starts again with `claude --bg --resume` and the
  name, model, effort and mode it last ran with.
- The session the panel server runs in takes no messages or End from here: closing it would stop the panel.

## Permission mode

A session's panel shows its permission mode (ask for each tool, accept file edits, plan, auto),
from its hook events or, after a panel restart, its transcript. Claude Code only changes a running
session's mode from its own screen (Shift+Tab) or with a permission answer, so:

- **A background session switches when you pick a mode** in the chat bar. `mode-switch.py`
  attaches to it in a pseudo-terminal (`claude attach`) and presses Shift+Tab until its footer shows
  that mode, in one burst so it doesn't sit in the modes in between, then closes. Not while it waits
  on you: Shift+Tab in an open dialog would answer it.
- **A terminal session** is on your screen, so Shift+Tab there. From the panel, the mode you pick in
  the chat bar is the one it carries on in once your message restarts it in the background.
- A main-agent permission request has a mode picker beside **Allow**: the allow also switches the
  session to it, the way an approved plan does (`setMode` for the session).

Bypass permissions is shown if a session is in it, but never offered, and a session whose Shift+Tab
cycle includes it isn't switched from here. The mode a session restarts in comes from your pick or
its transcript, never from a hook event: hooks carry no token.

## Model and effort

The chat bar also shows the model a session's replies run on (Opus 5.5 (1M), Sonnet 5.5, ...) and its effort level,
from the newest reply in its transcript. Pick another and it takes effect with your next message, which restarts the
session on it: Claude Code changes either for a running session only at its own prompt, and `/model` and `/effort`
there also make your pick the default for every new session. A restart (`claude --bg --resume ... --model --effort`)
changes this session alone.

- **A background session** restarts for it: **Restart and send** stops it if it is running, removes its job record
  (`claude rm`; the conversation stays), then resumes the same conversation under the same id with your message. While
  it works, the message waits until it is done. The record has to go because Claude Code resumes a listed job with the
  options it started with, and with any others starts a copy under a new id. One that works in its own worktree is
  not removed (that would take the worktree with it): it carries on as that copy, started from inside its worktree, so
  the same conversation goes on there under a new id, and the panel opens it. The old record stays on the board as
  ended and keeps the worktree; Claude Code refuses to remove it while the copy works there.
- **A terminal session** restarts with your message anyway, now on what you picked.
- A restart without a pick keeps what it ran on, `[1m]` included: the model is read as Claude Code asked for it
  (`requestedModel`), since the name a reply carries drops the 1M context.

The launcher has the same choices (**Model**, **Effort**) for a new agent.

## Slash commands

A message that starts with `/` is a command, run the way you would at the session's own prompt (`/remote-control` or
`/rc`, `/compact`, `/context`, a skill or a command of your own):

- **A background session that is running** takes no `/command` from outside (`claude --resume` refuses one), so
  `slash-command.py` types it at its prompt through `claude attach`, the way you would. It types only when Claude
  Code says the session is idle with nothing open and its screen shows an empty prompt. The command goes in with a
  space after its name, which closes the command list (Enter there would run whichever entry is highlighted). Then a
  fresh look at the screen must show exactly that command on the prompt line, nothing new above it, and the session
  still idle; only then does it press Enter, and otherwise it takes what it typed back off with Backspace. What the
  command printed shows in the chat. While the session works, the command waits until it is done (**Run when it is
  done**).
- **Any other session** gets it as the prompt it restarts with, as for a message.
- Refused here: a command that only opens a view or a menu on its screen (`/usage`, `/status`, `/config` without
  arguments, ...), which nobody would see in the background, and `/model` and `/effort` (use the menus). A command
  that opens a dialog anyway leaves the session waiting on it: the chat says so, and `claude attach` shows it.
- The first `/remote-control` ever asks once to confirm on the session's screen. Accept it there (`claude attach`) or
  in any terminal session; after that it connects without asking.

## Stopping and clearing sessions

Every control that stops something asks for a second click.

- **Interrupt** (a working session, or a single subagent in the panel): it stops at its next tool
  call, like pressing Esc. A session stays open for your next prompt; a subagent's session carries
  on without it. The panel refuses that next tool call through `PreToolUse`, and refuses a request
  that is waiting on you. A batch of tool calls already running still finishes.
- **End** (an idle session): `claude stop` for a background session; a terminal session gets
  SIGTERM, and Claude closes cleanly. The conversation is kept either way.
- **Remove** (a finished background session): `claude rm`. The transcript is kept. If its
  worktree has unpushed or uncommitted work, Claude Code refuses and it stays; the panel never
  forces it. A finished terminal session can only be **hidden** from the board.
- The queue headers have bulk versions: **Interrupt all** working sessions, **End idle**
  background sessions (terminal sessions are left alone, in case you're typing in one), and
  **Clear** finished ones.

## Sessions on a deleted version

Claude Code's updater deletes old versions while sessions still run them. Once the version a session runs is gone,
macOS can no longer name the program behind it when it checks a protected folder, so at its next check it refuses that
session, and everything it starts, `~/Documents`, `~/Desktop` and cloud folders ("Operation not permitted"). Granting
Full Disk Access doesn't help. A restart on the installed version does.

Every minute the panel checks each running session, in every profile, and the processes it was started from
(`versions.mjs`: one `ps` and one `lsof`, comparing each program file with what is on disk now).

- **A banner** over the board lists every one it finds, other profiles' sessions too: the one that loses access is
  often not the one on screen. A **toast** says when it finds a new one.
- **Its tile** says **restart**, and **its panel** says why and how.
- **To restart**: a terminal session quits in its terminal and resumes with the command the panel gives you to copy.
  A background job is ended in the panel, then sent a message, and comes back on the installed version.

## World

World starts when the service is installed, or, without the service, the first time you open it, as a bare rock called Home in the dark, and terraforms on the days you work
from then on, with lit space widening around Home when you zoom out (scroll, or **Home** and **Map**; drag to look
around). No agents in it: their work only arrives as credit. While it is open, its top bar says how many sessions need
you, and takes you back to the board at the first.

Beside Home sits its **sun**, placed by your world's seed: it lights Home's day side and the colonies', and marks your
galaxy in the Universe.

**Credit** is earned per calendar day, from the work you did that day:

    work   = lines changed + 20 × commits + 60 × active hours      (active hours count at most 10 a day)
    credit = round(65 × log2(1 + work ÷ 50))

Added and removed lines count the same, each of your commits is worth 20 lines on its own, and every active hour is
worth 60. The curve means more work always pays more, and each doubling of the work adds the same credit, so a big
day earns somewhat more, not many times more: 100 times the work is about 3 to 4 times the credit. There is no daily cap
beyond that. Some days:

| A day | Work | Credit |
|---|---|---|
| 15 minutes of anything | 15 | about 25 |
| 3 hours and one careful 30-line fix | 230 | about 160 |
| 4 hours investigating with an agent, nothing committed | 240 | about 165 |
| A typical full day: 8 active hours, 150 lines, 3 commits | 690 | about 250 |
| A huge day: 3,000 lines, 10 commits, 8 hours | 3,680 | about 400 |

Whether an agent or you wrote the code does not matter: the same work earns the same. **Tokens earn nothing.** They are
still counted and shown in the panel's today numbers, and a reply still marks its time active, but they add no credit
by themselves. Nothing needs to be committed for a day to count: a day spent on work that isn't committed yet still
earns for its active time.

**Active time** is counted in 5-minute slots of the local day, at most 10 hours (120 slots) a day. A slot counts when an
agent replied in it, or when the uncommitted files of a repository World counts changed in it. A slot counts once,
whatever moved in it, and an agent left running (a `/loop`, a scheduled job, a background session) marks its slots like
any other, so the 10 hours are the limit. **Time spent reading code, in a debugger or reviewing in a browser, with no
file changing and no agent working, is not counted**: World does not watch your editor, terminal or keyboard.

**Home levels up with XP** from the days you work. XP is never spent; credit is what you spend. A day's XP is a third of
its credit, rounded: about 85 for a typical full day, about 135 for a huge one. A day off pauses it, and XP never goes
back. The levels:

| Level | Home | XP |
|---|---|---|
| 1–3 | bare rock, clouds (within the first hour), oceans | 0, 15, 100 |
| 4–6 | continents, green coasts, forests | 300, 500, 800 |
| 7–9 | settlements with lights at night, cities under an atmosphere, terraformed with a moon | 1,200, 1,600, 2,000 |
| 10–12 | an orbital ring, a star port on it, twin moons | 2,600, 3,300, 4,000 |
| 13 and on | one small new feature at Home each, named after it | every 1,000 XP more |

Terraformed (level 9) takes about five weeks of typical weekdays, and Twin moons about ten. Past it Home keeps growing
(about 12 typical working days per level): Satellite, Bright air, Ring of lights, Near star, Aurora, Beacon, Second sun
glow, Comet trail, Moon base, Shield halo, Sky lanterns and Far lighthouse, then round again with a numeral (Beacon
II). Home's light on the map grows a little with each of them too. The box at World's top left shows the level, the XP
toward the next, and today's XP; the board says so when Home reaches a new level.

**Colonies grow by themselves.** A colony has six stages, from Landing site to City: Landing site, Outpost, Domes,
Green patches, Township, City. It is fed by half the XP Home earns after the colony landed, with nothing to feed by hand:
Outpost at 100 colony XP, Domes at 300, Green patches at 700, Township at 1,300 and City at 2,000, about 8 to 10 weeks
of typical work. After City it gains a feature every 1,000 colony XP more (Beacon tower, Orbital mirror, Harbour
lights, Rail ring, Garden roofs, Space lift, then round again with a numeral). A colony keeps its planet's look
(rust, ice, sand, violet) with the stages drawn on top, and builds sit on top of the stage. A colony founded earlier
grows from the XP earned since its own landing, so it can show up already grown. Claude's build prompt describes the
colony's stage as it does Home's level.

**What it costs.** A build costs 150, pushing the frontier 300, a probe 400 and a rocket 1,200: a build is under a day
of typical work, a push about a day, a probe a day and a half and a rocket about a working week. Every price is the
same however many of a kind you bought. A pushed region is fully lit after 600 more credit earned, and a probe arrives
after 500 more.

**Credit already earned is kept.** The first time this version runs, World records the day it switched, which is the
local day after it first runs (a World started fresh under this version uses the new formula from its first day). Days before it keep the credit and XP the old formula gave them, for good; days from it
on use the one above, so your balance never drops at the switch (it is the old earnings plus the new, minus everything
spent). A purchase keeps the price it was bought at, and a push or probe already under way keeps the light or travel credit it
was bought with (3,000 for one bought before this change), so it finishes as it was promised.

World counts the tokens, lines and commits like this:

- **Tokens** come from the transcripts of every Claude Code profile on this Mac, sessions and subagents: what was
  sent and written, once per reply. Cache reads don't count. There is one World, whichever profile the board shows.
- **Which repositories count**: World finds them from your agent sessions. When a session works in a folder, World takes
  that folder's git repository (a linked worktree counts as its main repository) and counts it from then on. There is
  no way to add a repository by hand, so someone who never uses an agent has none counted. In a counted repository
  everything counts, whoever made it: your commits (below), and its uncommitted file changes for active time.
- **Lines and commits** come from your commits in those repositories: commits whose
  author and committer are the `user.email` git uses there, on any branch or remote. Others' commits, merges, the
  stash, and squashes GitHub makes for you don't count, and neither do lockfiles; a file counts at most 2,000 lines
  each way per commit. A repository where git has no `user.email` counts nothing, and World says so; so does one
  whose history git doesn't read out in time.
- Each repository's lines per day are the most any count has seen there. So deleting a branch after it was merged
  keeps its credit, and an amended, reworded or rebased commit replaces its old count instead of adding to it. (A
  `fixup!` commit squashed away after World counted it keeps its lines.) Git is asked for the last 30 days; older
  days keep what was counted.
- Work from before the first open doesn't count. World is kept in
  `~/Library/Application Support/Agent Panel/world/world.json`. Git is only read.

**The rail** on World's right has a button for each thing you can do or look at: **Profile**, **Build**, **Send a
rocket**, **Push the frontier**, **Send a probe** (signed in to the hub), **Moments** and **Today's work**. Each opens its card beside the rail, one at a time;
click its button again, or ×, and the map has the whole window. World opens on Build the first time, then on the card
you had open last. A dot on the rocket means the frontier found a planet for a colony. Profile shows your last 100
days as a mosaic, a square a day and amber when your agents worked, signed in or not.

**Push the frontier** to spend credit. Pick where it leaves from, Home or one of your colonies (a chip each; the map
flies there), then a direction (from the list, or **Aim on the map**) and the dark that way lights up as you keep
working: a push opens 150 px of map and is fully lit after 600 more credit earned, about two or three normal days. A push
from a colony starts at the colony, so the map grows branches out to where you settled. A push from the same place
within 20° of an earlier one makes that stretch longer. Dark that the light walls in all round counts as lit, so
three branches that close a triangle fill it. The first push always finds a planet and each later one finds one half
the time; it shows once the light reaches it, and a click flies there. A push costs 300, every time. Light never
goes back.

**Moments** mark the map where they happened: each finished week with work (Monday to Sunday), each finished day that
removed 500 lines or more and more than it added ("Big cleanup", though removing no longer pays extra), and each Home level from 3 up. The Moments card lists
the newest; a click on one, or on its star, opens it there with what was done and flies to it.

**Send a rocket** to a bare planet the frontier found, and name the colony it lands (up to 24 letters, digits,
spaces, apostrophes or hyphens; not Home, and not another colony's name). The view pulls out, the rocket flies from
where you are, and the view flies in to the colony; with reduced motion it goes straight there. Colonies have a
button each beside **Home** and **Map**. A rocket costs 1,200, every time.

**Build** on Home or a colony: pick it in the Build card (a chip each, or its button beside **Home** and **Map**; either
flies there), describe it (up to 300 characters) and press **Build on** it, or Enter. Claude turns the
words into pieces World can draw: moons, rings, a ring city, city lights, land and ocean colours, an atmosphere,
mountains, domes, space stations, a space elevator, aurora and beacons, each with a colour, size, place and count.
Every build costs 150, wherever it goes and however many came before. Credit is taken only when the answer draws:
if Claude can't be reached, the profile isn't signed in, or the answer doesn't fit, nothing is spent and the Build
card says why. One build at a time.

That call is the only time World uses Claude: `claude -p --model haiku` with a JSON schema, under the profile the
board shows, with hooks off, no tools, MCP servers or slash commands, and no session saved. It runs in
`~/Library/Application Support/Agent Panel/world/build`, and the board leaves it out
([design](docs/superpowers/specs/2026-10-21-world-design.md)).

## Hub

The hub is a small website that gives each panel a page of its own: your galaxy, drawn the way World draws it, which
anyone with your handle or link can open. Your galaxy has fixed coordinates on the hub, shown on your page, and
other people's galaxies are out there around it, to be found with probes (below).
You choose whether to sign in. The Mac sends nothing to the hub until it is signed in (by the one line run from the
Download page while signed in, by `node server.mjs login`, or from the panel's Profile box), except the worker's
daily release check: a plain GET with no token, followed by a download from GitHub. The panel's own work stays local
either way.

**Sign in** from the **Profile** card in World's rail (**Sign in with GitHub**), or from a terminal:

```
node server.mjs login      # prints a code and a link, and opens the link
node server.mjs status     # signed in as whom, whether the site may place orders, and whether the background service runs
node server.mjs logout     # revokes this Mac's token on the hub and forgets it
```

Either way you get a code like `BKQT-7W3N` and a link. Open the link, sign in to the hub with GitHub if it asks, and
approve the code; the panel notices and says who you are. The code runs out after 15 minutes. **Sign out** in the
Profile card does what `logout` does. Your sign-in is kept in `hub.json`, beside `prefs.json` in
`~/Library/Application Support/Agent Panel`. If you sign in from a terminal while the background service runs, the
service picks it up.

**What is shared** is the shape of your galaxy, and only that:

- Your planets, the colonies on them with their names, your builds with their captions and pieces, your moments (with
  their day and kind), Home's level and stage, how many days you have been active, which of the last 100 days had work in them, and your sun.
- Your probes: the direction each was sent, how far along it is, and, once it has arrived, the galaxy it found. Your
  page shows its own Universe, so anyone on it sees the galaxies you found. To aim a probe the hub is told its
  direction and the galaxies you have already found.
- A small profile you write on the hub: your name, a bio, a link. Your GitHub handle and picture come with it. On the
  same page you can set your galaxy to **unlisted**: it still opens for anyone with the link, but search engines are
  told to leave it out and it cannot be found by its coordinates.

**What never leaves your Mac**: your repositories and their paths, build prompts, and any time finer than a day. Of how
World has grown, only Home's level and each colony's stage go on your public page (as numbers; the hub names them
itself). Tokens, lines, credit, XP, active time and prices are never on it.

Your Mac also tells the hub its own state so your page can show it to you and nobody else: your balance and XP, the
prices, today's numbers (credit, lines, commits, tokens, active time), whether a build is running, and the Mac's name.
That is shown only to you, signed in.

The panel sends about ten seconds after something changes, a second after a purchase or an order, and once every
30 minutes otherwise, so your page knows the Mac is still there. A hub that is down never slows the panel: it tries again later.

**The Universe** is the zoom past **Map**, signed in: your galaxy, drawn as a small spiral, in the dark. You start
alone and find the others by sending probes from the **Send a probe** card. Pick a direction (from the list, or **Aim
on the map**) and the hub aims the probe at the nearest galaxy that way you haven't found: the nearest within 60° of
where you pointed, or if there is none, the nearest anywhere. A probe costs 400 and arrives after 500 more credit
earned; on its way it shows as a dot heading out the way you sent it. When it arrives that galaxy appears, with its
handle, and a click opens its page. A probe always finds someone; if there is no one left to find, the card says so
and nothing is spent. Unlisted galaxies are never found, and a galaxy that is found isn't told. Signed out, there is
no Universe.

**Your page** is `<hub>/@handle`. `<hub>/me` is where you edit your profile: handle, name, bio, link, and whether you are
listed. Unlisted asks search engines not to index your page; a link still opens it.

**Spending from your page.** Signed in on the hub, you can push the frontier, send a rocket, build and send a probe
from your page, and your Mac does it: **Your orders** on the page's rail, which only you see, has Build, Rocket, Push
and Probe as tabs, and the Build and Push chips fly the map to the place the build goes on or the push leaves from.
A Mac whose panel predates probes is asked for neither a probe nor a push from a colony: update it first. An order from the site goes through the same code as the panel's buttons, so the same prices
and rules apply, and the page shows what happened, or the Mac's own sentence when it said no ("It costs 400 credit,
and you have 320."). One order at a time. With two Macs signed in to one account, an order is run by whichever
signed-in Mac claims it first, and an order whose Mac went away while running it is released after an hour. If the
Mac is asleep or offline, the order waits and runs when it wakes (the background service does this with no panel
open); an order not picked up within 24 hours expires.

**Your Mac decides what the site may do.** The hub only queues orders; the Mac runs them, and only within limits kept
on the Mac, in `web-orders.json` beside `prefs.json`, which nothing from the hub can change. It runs the four kinds above
and nothing else. A build asks Claude, on this Mac's account, for drawing pieces only: the call has no tools (each one
that could read, write, run or fetch is also denied by name), no MCP servers, hooks or skills, and an answer that shows
Claude tried anything more is thrown away. At most 10 builds a day come from the site (set `"buildsPerDay"` in the
file to change it), and every order the Mac runs or refuses shows a notification, so one you didn't place is seen at
once. To stop the site placing orders on this Mac at all, or allow it again:

```
node server.mjs orders off   # orders from the site are refused here, with a sentence the page shows
node server.mjs orders on
node server.mjs orders       # on or off, and today's builds
```

**Only want World?** Open `http://127.0.0.1:4747/world`: the page opens straight into World, without the board. A
bookmark can point there.

**Which hub.** The panel talks to the Universe at `https://universe.cashegg.com` (`DEFAULT_HUB` in `hub.mjs`). To
use another, such as a hub on this machine while developing (`http://127.0.0.1:4848`), set `ACP_HUB` in the
environment of whatever runs the panel or `node server.mjs login`. A
sign-in belongs to the hub that issued it: pointed at a different hub, the panel shows you as signed out, and your old
sign-in is still in `hub.json` for when you point back. To run a hub of your own, see [`hub/README.md`](hub/README.md).

## Security

An endpoint that approves commands is worth protecting, even on localhost:

- It listens on `127.0.0.1` only and refuses any other `Host` header, which blocks DNS rebinding.
- Browser calls need a random token minted at each start, plus a same-origin `Origin`. That
  blocks other tabs from posting decisions.
- Hook endpoints refuse anything that carries an `Origin` header, so a web page can't inject fake requests.
- Hook endpoints take no token, so any program on this Mac can post them a made-up event. That changes only what the
  panel shows: every action checks Claude Code's own records again first (`claude agents`, the session's transcript),
  and a made-up permission request, if you answer it, answers only the program that sent it. A program running as you
  can already do anything you can; a shared key would have to sit in your Claude Code settings file, which other
  accounts on the Mac can usually read, so it wouldn't keep them out either.
- Nothing a request carries can stop the server: the `Host` is checked before anything else, and an error in one
  request ends that request alone.
- Launch, messages, mode switches, folder suggestions, the folder dialog, interrupt, end and remove
  take the same token and Origin. They run `claude`
  through argv with no shell, and check every value first: the folder must exist, the model and
  permission mode come from a fixed list (no bypass-permissions), and names and agent types have
  a fixed character set.
- Ending a terminal session signals only a pid read fresh from `claude agents`, only when that
  process is still `claude`, and never the session the panel server itself runs in.
- `claude` is started without the `CLAUDE*` variables of the session that started the server
  (except `CLAUDE_CONFIG_DIR`), so a launched session doesn't take itself for a child of it.
- The hub token is kept in `hub.json` beside `prefs.json` (mode 0600) and is sent only as a bearer to the hub you signed
  in to. The page in your browser never talks to the hub: the local server does, for the page, behind the same token
  and Origin checks as everything else. There is no CORS on either side.
- Nothing leaves your Mac until it is signed in to the hub; after that, what leaves is an allow-list projection of
  World, copied key by key (a field added later stays private until it is listed) and checked again on the hub before
  it is stored. The hub stores tokens and session ids hashed; the one exception is a token you have just approved,
  which the hub holds in clear only until your Mac collects it or the code runs out (15 minutes).
- The worker only ever installs a commit the hub names that is signed by a key listed in `release-keys.mjs` (the
  private half lives on the maintainer's Mac, encrypted), is on the repository's `main` **and descends from the release
  it runs**, so neither a stolen key nor a stolen server can send it back to older code; downloaded from GitHub over
  https, verified before the download.
- The hub rate-limits every sign-in, upload and order, per visitor address or per account, and answers `429` with a
  `retry-after` when a limit is reached (`hub/README.md` has the table). A galaxy set to unlisted is reached by its link
  only: crawlers are told not to index it and a scan of the map's coordinates does not find it.

## Uninstall

### The worker alone

Sign out first, so the hub forgets this Mac's token, then remove the worker:

```bash
node "$HOME/Library/Application Support/Agent Panel/worker/current/server.mjs" logout
curl -fsSL https://raw.githubusercontent.com/IcyDingo/agent-control-panel/main/hub/public/install.sh | sh -s -- --uninstall
```

### The app too

Run `./install.sh --uninstall` from the clone: it removes the background service and
`~/Applications/Agent Panel.app`. If you signed in to the hub, run `node server.mjs logout` before it: it revokes the
hub token. Then remove the hook entries that point at `127.0.0.1:4747` from each profile's settings, and delete
`~/Library/Application Support/Agent Panel` and `~/Library/Logs/Agent Panel`. Deleting the Application Support folder
also removes what the worker left there, including `hub.json` and your World file.
