//! The TCP listener (plan 5): `127.0.0.1:<port>` and, with `--listen tailscale`, this machine's Tailscale IPv4. Never
//! `0.0.0.0`, never another interface, never IPv6. A peer is allowed or refused from its address before a byte of it is
//! read as a frame (Rulings P5-R2, P5-R3, P5-R7):
//! - a loopback peer sends the key preamble (`FKEY` and the 32 key bytes), or on Linux is this user's socket
//!   (`/proc/net/tcp`);
//! - a Tailscale peer is this machine's own Tailscale user on an untagged node, by `tailscale whois`. The nodes
//!   `tailscale status` lists are pre-allowed when the listener starts, and a decision is kept 60 s; a kept allow older
//!   than that is used at once and checked again in the background (a deny then closes the connection).
//!
//! An allowed peer then has exactly the Unix socket's conversation (`serve::handle`). A refused one is sent one `Error`
//! (`not allowed: <why>`) and closed.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fleet_model::wire::{encode, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;

use crate::config::{Config, Listen};
use crate::tailscale;

/// The key preamble's first 4 bytes. As a little-endian frame length it is ~1.26 GB, over `MAX_FRAME`: never a frame.
pub const FKEY: &[u8; 4] = b"FKEY";

/// How long a loopback peer has to send its first 4 bytes.
const KEY_WITHIN: Duration = Duration::from_secs(10);
/// How long `tailscale whois` (and `status`) may take.
const WHOIS_WITHIN: Duration = Duration::from_secs(2);
/// How long a decision is kept (allow and deny alike).
const KEEP_FOR: Duration = Duration::from_secs(60);
/// The most connections in auth at once; past that a connection is refused at once (`busy`).
const MAX_IN_AUTH: usize = 16;
/// A peer's refusals are logged at most once per this.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// While the Tailscale address is not bound, how often it is tried again.
const TS_RETRY: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------------------------------------- sockets

/// The keepalive timing: idle 15 s, then a probe every 5 s, 3 probes (about 30 s to notice a peer gone without a FIN).
/// Debug builds take `FLEETD_TEST_KEEPALIVE_SECS` for both the idle and the interval (Ruling P5-R5).
fn keepalive_times() -> (Duration, Duration) {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("FLEETD_TEST_KEEPALIVE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        return (Duration::from_secs(s), Duration::from_secs(s));
    }
    (Duration::from_secs(15), Duration::from_secs(5))
}

/// Sets `TCP_NODELAY` and the keepalive on a TCP socket: every accepted one, and every one `Client::connect_tcp` dials.
pub fn keepalive<S: AsFd>(s: &S) -> io::Result<()> {
    let s = socket2::SockRef::from(s);
    s.set_tcp_nodelay(true)?;
    let (idle, interval) = keepalive_times();
    let ka = socket2::TcpKeepalive::new()
        .with_time(idle)
        .with_interval(interval)
        .with_retries(3);
    s.set_tcp_keepalive(&ka)
}

/// In `100.64.0.0/10`, the tailnet's range.
pub fn in_tailnet(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && o[1] & 0xc0 == 64
}

/// Debug builds only: `FLEETD_TEST_TS_RANGE_ANY=1` lifts the `100.64/10` check, so a test can stand a loopback address in
/// for the Tailscale one.
fn range_any() -> bool {
    cfg!(debug_assertions) && std::env::var("FLEETD_TEST_TS_RANGE_ANY").as_deref() == Ok("1")
}

fn ts_retry() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("FLEETD_TEST_TS_RETRY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    TS_RETRY
}

/// The IPv4 addresses assigned to this machine's interfaces.
pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    interfaces().into_iter().map(|(ip, _, _)| ip).collect()
}

/// Each interface IPv4: (address, netmask, is loopback).
fn interfaces() -> Vec<(Ipv4Addr, Ipv4Addr, bool)> {
    let mut out = vec![];
    let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list we free with freeifaddrs; each entry is read only while the list lives.
    unsafe {
        if libc::getifaddrs(&mut ifa) != 0 {
            return out;
        }
        let mut p = ifa;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let m = (*p).ifa_netmask;
                let mask = if m.is_null() {
                    Ipv4Addr::BROADCAST
                } else {
                    let sm = &*(m as *const libc::sockaddr_in);
                    Ipv4Addr::from(u32::from_be(sm.sin_addr.s_addr))
                };
                let lo = (*p).ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0;
                out.push((Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)), mask, lo));
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}

/// Whether `ip` may be bound as the Tailscale address: not unspecified, in the tailnet range, on a local interface.
fn usable(ip: Ipv4Addr, any: bool) -> Result<(), String> {
    if ip.is_unspecified() {
        return Err("the unspecified address".into());
    }
    if !any && !in_tailnet(ip) {
        return Err("not in 100.64.0.0/10".into());
    }
    if !on_local_interface(ip) {
        return Err("not on a local interface".into());
    }
    Ok(())
}

/// Assigned to an interface, or inside a loopback interface's subnet (Linux's lo answers all of 127/8; that is the only
/// case, and only a debug build's stand-in can ask for it).
fn on_local_interface(ip: Ipv4Addr) -> bool {
    let ip_n = u32::from(ip);
    interfaces()
        .into_iter()
        .any(|(a, m, lo)| a == ip || (lo && u32::from(a) & u32::from(m) == ip_n & u32::from(m)))
}

fn std_bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let l = std::net::TcpListener::bind(addr)?;
    l.set_nonblocking(true)?;
    TcpListener::from_std(l)
}

fn secs(d: Duration) -> String {
    format!("{}s", d.as_secs_f64())
}

/// Binds what `cfg.listen` asks for (nothing for `Off`): `127.0.0.1:<port>` first, then, for `Tailscale`, the Tailscale
/// IPv4 on the same port, if it can be had now (otherwise `tailscale_later` keeps trying). A loopback that cannot be
/// bound is an error; the Tailscale address is not. Needs a runtime context.
pub fn listen(cfg: &Config, auth: Arc<Auth>) -> io::Result<Vec<TcpListener>> {
    if cfg.listen == Listen::Off {
        return Ok(vec![]);
    }
    let lo = std_bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), cfg.port))
        .map_err(|e| io::Error::new(e.kind(), format!("tcp 127.0.0.1:{}: {e}", cfg.port)))?;
    let port = lo.local_addr()?.port();
    eprintln!("fleetd: listening tcp {}", lo.local_addr()?);
    let mut out = vec![lo];
    if cfg.listen == Listen::Tailscale {
        match bind_tailscale(port, &auth) {
            Ok(Some(l)) => out.push(l),
            Ok(None) => {}
            Err(why) => eprintln!("fleetd: tailscale: {why}; retrying in {}", secs(ts_retry())),
        }
    }
    Ok(out)
}

/// The Tailscale listener on `port`, tried every 30 s until it is bound (a log line each time the reason changes).
/// `None` when the address turned out to be one already listened on (only a test's loopback stand-in can be).
pub async fn tailscale_later(port: u16, auth: Arc<Auth>) -> Option<TcpListener> {
    let mut last = None;
    loop {
        tokio::time::sleep(ts_retry()).await;
        let a = auth.clone();
        let r = tokio::task::spawn_blocking(move || bind_tailscale(port, &a))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match r {
            Ok(l) => return l,
            Err(why) => {
                if last.as_ref() != Some(&why) {
                    eprintln!("fleetd: tailscale: {why}; retrying in {}", secs(ts_retry()));
                }
                last = Some(why);
            }
        }
    }
}

fn bind_tailscale(port: u16, auth: &Auth) -> Result<Option<TcpListener>, String> {
    let Some(ts) = &auth.whois else {
        return Err("no tailscale CLI".into());
    };
    let ip = ts.ipv4().map_err(|e| format!("ip -4: {e}"))?;
    usable(ip, auth.range_any).map_err(|why| format!("not binding {ip}: {why}"))?;
    let addr = SocketAddr::new(ip.into(), port);
    if ip == Ipv4Addr::LOCALHOST {
        // A test's stand-in (debug builds only), on macOS, whose lo0 has no other loopback address: the loopback listener
        // is this address already. On Linux the tests stand in 127.0.0.2, a real second bind.
        auth.set_tailscale(ip);
        eprintln!("fleetd: listening tcp {addr} (tailscale)");
        return Ok(None);
    }
    let l = {
        // Bound from a blocking thread: from_std needs the runtime's context.
        let l = std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
        l.set_nonblocking(true).map_err(|e| e.to_string())?;
        l
    };
    let l = match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            let _in = h.enter();
            TcpListener::from_std(l).map_err(|e| e.to_string())?
        }
        Err(_) => return Err("no runtime".into()),
    };
    auth.set_tailscale(ip);
    eprintln!("fleetd: listening tcp {addr} (tailscale)");
    Ok(Some(l))
}

// ------------------------------------------------------------------------------------------------- auth

/// What auth asks the tailnet, as a trait so the tests need no process. `tailscale::Cli` is the real one.
pub trait WhoIs: Send + Sync {
    fn ipv4(&self) -> io::Result<Ipv4Addr>;
    fn status(&self) -> io::Result<tailscale::Status>;
    fn whois(&self, peer: SocketAddr) -> io::Result<tailscale::Peer>;
}

impl WhoIs for tailscale::Cli {
    fn ipv4(&self) -> io::Result<Ipv4Addr> {
        tailscale::Cli::ipv4(self)
    }
    fn status(&self) -> io::Result<tailscale::Status> {
        tailscale::Cli::status(self)
    }
    fn whois(&self, peer: SocketAddr) -> io::Result<tailscale::Peer> {
        tailscale::Cli::whois(self, peer)
    }
}

/// An allowed connection. A Tailscale one carries `revoked`, its peer IP's revocation: it fires (with the reason) when a
/// re-check of that IP now denies it, and every connection from that IP is then closed.
#[derive(Debug)]
pub struct Allowed {
    pub revoked: Option<Revoked>,
}

/// One connection's view of its peer IP's revocation.
#[derive(Debug)]
pub struct Revoked(watch::Receiver<Option<String>>);

impl Revoked {
    /// The reason, once the peer is revoked; never, if it is not.
    pub async fn wait(mut self) -> String {
        loop {
            if let Some(why) = self.0.borrow_and_update().clone() {
                return why;
            }
            if self.0.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

struct Kept {
    at: Instant,
    verdict: Result<(), String>,
    rechecking: bool,
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The decision: who may talk to this daemon over TCP.
pub struct Auth {
    key: Option<[u8; 32]>,
    whois: Option<Arc<dyn WhoIs>>,
    self_user: Mutex<Option<u64>>,
    kept: Mutex<HashMap<IpAddr, Kept>>,
    logged: Mutex<HashMap<IpAddr, Instant>>,
    in_auth: AtomicUsize,
    tailscale: Mutex<Option<Ipv4Addr>>,
    /// This machine's own tailnet addresses (`Self.TailscaleIPs`): local, never a tailnet peer (Ruling P5-R9).
    self_ips: Mutex<Vec<IpAddr>>,
    /// Per peer IP, the revocation every connection allowed from it waits on.
    revokers: Mutex<HashMap<IpAddr, watch::Sender<Option<String>>>>,
    clock: Clock,
    range_any: bool,
    /// The tests' switch: no address counts as this machine's own, so a loopback connection can stand in for a peer.
    nothing_local: bool,
}

/// One connection in auth, counted until dropped.
pub struct InAuth(Arc<Auth>);

impl Drop for InAuth {
    fn drop(&mut self) {
        self.0.in_auth.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Auth {
    /// `key` is the loopback key; `whois` the tailnet (none: every Tailscale peer is refused). With a tailnet, its status
    /// is read once now (blocking, at most 2 s): this machine's user, and the nodes to pre-allow.
    pub fn new(key: Option<[u8; 32]>, whois: Option<Arc<dyn WhoIs>>) -> Auth {
        let a = Auth {
            key,
            whois,
            self_user: Mutex::new(None),
            kept: Mutex::new(HashMap::new()),
            logged: Mutex::new(HashMap::new()),
            in_auth: AtomicUsize::new(0),
            tailscale: Mutex::new(None),
            self_ips: Mutex::new(vec![]),
            revokers: Mutex::new(HashMap::new()),
            clock: Arc::new(Instant::now),
            range_any: range_any(),
            nothing_local: false,
        };
        if let Some(w) = &a.whois {
            match w.status() {
                Ok(s) => a.learn(&s),
                Err(e) => eprintln!("fleetd: tailscale: status: {e}"),
            }
        }
        a
    }

    /// The same, on another clock (the tests' mocked one).
    pub fn with_clock(mut self, clock: Clock) -> Auth {
        let now = clock();
        for k in self.kept.get_mut().unwrap().values_mut() {
            k.at = now;
        }
        self.clock = clock;
        self
    }

    /// Takes this machine's user and own addresses from `status`, and pre-allows each other node of that user that is
    /// untagged. Self is never pre-allowed: a connection from it is local (Ruling P5-R9).
    fn learn(&self, s: &tailscale::Status) {
        *self.self_user.lock().unwrap() = Some(s.self_user);
        {
            let mut mine = self.self_ips.lock().unwrap();
            for n in s.nodes.iter().filter(|n| n.is_self) {
                for ip in &n.ips {
                    if !mine.contains(ip) {
                        mine.push(*ip);
                    }
                }
            }
        }
        let now = (self.clock)();
        let mut kept = self.kept.lock().unwrap();
        for n in s.nodes.iter().filter(|n| pre_allowed(s, n)) {
            for ip in n.ips.iter().filter(|ip| ip.is_ipv4()) {
                if let IpAddr::V4(v4) = ip {
                    if self.is_local(*v4) {
                        continue;
                    }
                }
                kept.entry(*ip).or_insert(Kept {
                    at: now,
                    verdict: Ok(()),
                    rechecking: false,
                });
            }
        }
    }

    /// The nodes pre-allowed now: (node, allowed), for `fleetd tailnet`.
    pub fn listing(s: &tailscale::Status) -> Vec<(&tailscale::Known, bool)> {
        s.nodes.iter().map(|n| (n, pre_allowed(s, n))).collect()
    }

    /// A source address of this machine's own (Ruling P5-R9): loopback, an interface's, the bound Tailscale address, or
    /// one of `Self.TailscaleIPs`. A peer from one is local on either listener.
    fn is_local(&self, ip: Ipv4Addr) -> bool {
        if self.nothing_local {
            return false;
        }
        ip.is_loopback()
            || self.tailscale_addr() == Some(ip)
            || self.self_ips.lock().unwrap().contains(&IpAddr::V4(ip))
            || local_ipv4s().contains(&ip)
    }

    /// A connection from `ip`'s share in that IP's revocation.
    fn subscribe(&self, ip: IpAddr) -> Revoked {
        let mut r = self.revokers.lock().unwrap();
        if r.len() > 256 {
            r.retain(|_, tx| tx.receiver_count() > 0);
        }
        Revoked(
            r.entry(ip)
                .or_insert_with(|| watch::channel(None).0)
                .subscribe(),
        )
    }

    /// Closes every connection allowed from `ip`.
    fn revoke(&self, ip: IpAddr, why: &str) {
        if let Some(tx) = self.revokers.lock().unwrap().remove(&ip) {
            let _ = tx.send(Some(why.to_string()));
        }
    }

    fn set_tailscale(&self, ip: Ipv4Addr) {
        *self.tailscale.lock().unwrap() = Some(ip);
    }

    /// The Tailscale address once it is bound.
    pub fn tailscale_addr(&self) -> Option<Ipv4Addr> {
        *self.tailscale.lock().unwrap()
    }

    /// A place in auth, or `None` when 16 connections are in auth already.
    pub fn enter(self: &Arc<Self>) -> Option<InAuth> {
        let n = self.in_auth.fetch_add(1, Ordering::SeqCst);
        let g = InAuth(self.clone());
        (n < MAX_IN_AUTH).then_some(g)
    }

    /// Logs a refusal, once per peer per minute. Never the key's bytes: `why` is one of auth's own words.
    pub fn log_refusal(&self, ip: IpAddr, why: &str) {
        let now = (self.clock)();
        let mut logged = self.logged.lock().unwrap();
        if logged
            .get(&ip)
            .is_some_and(|at| now.duration_since(*at) < LOG_EVERY)
        {
            return;
        }
        if logged.len() > 256 {
            logged.retain(|_, at| now.duration_since(*at) < LOG_EVERY);
        }
        logged.insert(ip, now);
        eprintln!("fleetd: tcp refused {ip}: {why}");
    }

    /// Allows or refuses a connection from `peer` to `local` before anything of it is read as a frame. A loopback peer's
    /// key preamble, if it sends one, is taken off the stream; nothing else is.
    pub async fn check(
        self: &Arc<Self>,
        peer: SocketAddr,
        local: SocketAddr,
        stream: &TcpStream,
    ) -> Result<Allowed, String> {
        let local_ip = match local.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return Err("not a listening address".into()),
        };
        let IpAddr::V4(peer_ip) = peer.ip() else {
            return Err("not a tailnet address".into());
        };
        // A process of this machine's, whichever address it dialled, is local: the key, or the uid (Ruling P5-R9).
        if self.is_local(peer_ip) {
            self.loopback(peer, local, stream).await?;
            return Ok(Allowed { revoked: None });
        }
        if Some(local_ip) == self.tailscale_addr() {
            return self.tailnet(peer).await;
        }
        Err("not a listening address".into())
    }

    /// A local peer: the key preamble, else (Linux only) this user's own socket.
    async fn loopback(
        &self,
        peer: SocketAddr,
        local: SocketAddr,
        stream: &TcpStream,
    ) -> Result<(), String> {
        let head = match timeout(KEY_WITHIN, peek(stream, 4)).await {
            Err(_) => return Err("nothing sent".into()),
            Ok(Err(e)) => return Err(format!("read: {e}")),
            Ok(Ok(h)) => h,
        };
        if head == FKEY[..] {
            let all = match timeout(KEY_WITHIN, peek(stream, 4 + 32)).await {
                Err(_) => return Err("a cut key".into()),
                Ok(Err(e)) => return Err(format!("read: {e}")),
                Ok(Ok(a)) => a,
            };
            let ok = self
                .key
                .is_some_and(|k| k.iter().zip(&all[4..]).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0);
            if !ok {
                return Err("wrong key".into());
            }
            return take(stream, 36).await.map_err(|e| format!("read: {e}"));
        }
        if same_user(peer, local) {
            return Ok(());
        }
        Err("loopback needs the key".into())
    }

    /// A Tailscale peer (Rulings P5-R3, P5-R7): a kept decision (an allow older than 60 s is checked again in the
    /// background), else `tailscale whois` now.
    pub async fn tailnet(self: &Arc<Self>, peer: SocketAddr) -> Result<Allowed, String> {
        let ip = match peer.ip() {
            IpAddr::V4(ip) if self.range_any || in_tailnet(ip) => ip,
            _ => return Err("not a tailnet address".into()),
        };
        if self.is_local(ip) {
            return Err("a local peer needs the key".into());
        }
        if self.whois.is_none() {
            return Err("no tailscale CLI".into());
        }
        let now = (self.clock)();
        let recheck = {
            let mut kept = self.kept.lock().unwrap();
            match kept.get_mut(&IpAddr::V4(ip)) {
                Some(k) if k.verdict.is_err() && now.duration_since(k.at) < KEEP_FOR => {
                    return Err(k.verdict.clone().unwrap_err());
                }
                Some(k) if k.verdict.is_ok() => {
                    let stale = now.duration_since(k.at) >= KEEP_FOR && !k.rechecking;
                    if stale {
                        k.rechecking = true;
                    }
                    Some(stale)
                }
                _ => None,
            }
        };
        match recheck {
            Some(false) => Ok(Allowed {
                revoked: Some(self.subscribe(IpAddr::V4(ip))),
            }),
            Some(true) => {
                let revoked = self.subscribe(IpAddr::V4(ip));
                let me = self.clone();
                tokio::spawn(async move {
                    let v = me.decide(peer).await;
                    let mut kept = me.kept.lock().unwrap();
                    match v {
                        Ok(()) => {
                            kept.insert(
                                IpAddr::V4(ip),
                                Kept {
                                    at: (me.clock)(),
                                    verdict: Ok(()),
                                    rechecking: false,
                                },
                            );
                        }
                        // A deny now: the entry goes, and so does every connection from this IP.
                        Err(Deny::Is(why)) => {
                            kept.remove(&IpAddr::V4(ip));
                            drop(kept);
                            me.log_refusal(IpAddr::V4(ip), &why);
                            me.revoke(IpAddr::V4(ip), &why);
                        }
                        // The CLI could not say (slow, gone): the allow stands, unrefreshed, and is asked again next time.
                        Err(Deny::Unknown(_)) => {
                            if let Some(k) = kept.get_mut(&IpAddr::V4(ip)) {
                                k.rechecking = false;
                            }
                        }
                    }
                });
                Ok(Allowed {
                    revoked: Some(revoked),
                })
            }
            None => {
                let v = self.decide(peer).await.map_err(Deny::why);
                self.kept.lock().unwrap().insert(
                    IpAddr::V4(ip),
                    Kept {
                        at: (self.clock)(),
                        verdict: v.clone(),
                        rechecking: false,
                    },
                );
                v.map(|()| Allowed {
                    revoked: Some(self.subscribe(IpAddr::V4(ip))),
                })
            }
        }
    }

    /// WhoIs now: the same user as this machine's (re-read once on a mismatch), on an untagged node.
    async fn decide(&self, peer: SocketAddr) -> Result<(), Deny> {
        let w = self
            .whois
            .clone()
            .ok_or(Deny::Is("no tailscale CLI".into()))?;
        let p = blocking(WHOIS_WITHIN, {
            let w = w.clone();
            move || w.whois(peer)
        })
        .await
        .map_err(|e| {
            Deny::Unknown(if e.kind() == io::ErrorKind::TimedOut {
                "whois timed out".into()
            } else {
                format!("whois: {e}")
            })
        })?;
        if p.tagged {
            return Err(Deny::Is("a tagged node".into()));
        }
        let mine = *self.self_user.lock().unwrap();
        if mine == Some(p.user) {
            return Ok(());
        }
        // This machine's user may have changed (a re-login): read once more.
        if let Ok(s) = blocking(WHOIS_WITHIN, move || w.status()).await {
            self.learn(&s);
            if s.self_user == p.user {
                return Ok(());
            }
        }
        Err(Deny::Is("another tailnet user".into()))
    }
}

/// A node `status` lists is pre-allowed when it is this machine's user's and untagged (Ruling P5-R7), and is not this
/// machine itself (Ruling P5-R9: a connection from Self is local and needs the key).
pub fn pre_allowed(s: &tailscale::Status, n: &tailscale::Known) -> bool {
    !n.is_self && n.user == s.self_user && !n.tagged
}

/// Why a peer is not allowed: it is not (`Is`), or the CLI could not say (`Unknown`; on a first connect that is a
/// refusal too).
enum Deny {
    Is(String),
    Unknown(String),
}

impl Deny {
    fn why(self) -> String {
        match self {
            Deny::Is(w) | Deny::Unknown(w) => w,
        }
    }
}

/// `f` on a thread of its own, given up on (with `TimedOut`) past `within`. Not the runtime's blocking pool, so a stuck
/// call never holds the runtime's shutdown.
async fn blocking<T: Send + 'static>(
    within: Duration,
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match timeout(within, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err(io::Error::other("the call panicked")),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
    }
}

/// The first `n` bytes the peer sent, left in the stream. `UnexpectedEof` if it closed first.
/// Waits on the socket's readiness, never a timer: while fewer than `n` bytes are there, the peek reports `WouldBlock`,
/// which clears the readiness, so the next wait is for more bytes to arrive.
async fn peek(stream: &TcpStream, n: usize) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut buf = vec![0u8; n];
    let fd = stream.as_raw_fd();
    loop {
        stream.readable().await?;
        let got = stream.try_io(tokio::io::Interest::READABLE, || {
            // SAFETY: a recv into our own buffer of `n` bytes on the stream's fd, which it keeps open.
            let k = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), n, libc::MSG_PEEK) };
            match k {
                k if k < 0 => Err(io::Error::last_os_error()),
                0 => Ok(0),
                k if (k as usize) < n => Err(io::ErrorKind::WouldBlock.into()),
                k => Ok(k as usize),
            }
        });
        match got {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => return Ok(buf),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Takes `n` bytes already peeked off the stream.
async fn take(stream: &TcpStream, mut n: usize) -> io::Result<()> {
    let mut buf = [0u8; 64];
    while n > 0 {
        stream.readable().await?;
        match stream.try_read(&mut buf[..n.min(64)]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(k) => n -= k,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Whether the loopback peer's socket is this user's (Ruling P5-R2): Linux reads its owner from `/proc/net/tcp`; macOS
/// has no cheap per-socket lookup, so never.
fn same_user(peer: SocketAddr, local: SocketAddr) -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        ["/proc/net/tcp", "/proc/net/tcp6"].iter().any(|t| {
            std::fs::read_to_string(t)
                .ok()
                .and_then(|text| proc_tcp_uid(&text, peer, local))
                == Some(me)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (peer, local);
        false
    }
}

/// The uid owning the socket `peer` → `local` in a `/proc/net/tcp{,6}` table: the row whose local address is `peer` and
/// remote is `local` (the peer's own socket, not the accepted one).
pub fn proc_tcp_uid(text: &str, peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    text.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 8 {
            return None;
        }
        (proc_addr(f[1])? == peer && proc_addr(f[2])? == local).then(|| f[7].parse().ok())?
    })
}

/// `0100007F:1F90` (IPv4) or 32 hex digits and a port (IPv6, read only when it maps an IPv4): the kernel prints each
/// 32-bit word of the address in host order.
fn proc_addr(s: &str) -> Option<SocketAddr> {
    let (a, p) = s.split_once(':')?;
    let port = u16::from_str_radix(p, 16).ok()?;
    let word = |h: &str| u32::from_str_radix(h, 16).ok().map(u32::to_ne_bytes);
    let ip = match a.len() {
        8 => Ipv4Addr::from(word(a)?),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(&a[i * 8..i * 8 + 8])?);
            }
            std::net::Ipv6Addr::from(b).to_ipv4_mapped()?
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip.into(), port))
}

/// Refuses a connection: one `Error { "not allowed: <why>" }`, a FIN, then whatever the peer still sends is thrown away
/// (unread as frames) for up to a second, so the close is not a reset that could cost it the `Error`.
pub async fn refuse(mut stream: TcpStream, why: &str) {
    let message = format!("not allowed: {why}");
    if let Ok(b) = encode(&Frame::Error { message }) {
        let _ = timeout(Duration::from_secs(5), stream.write_all(&b)).await;
    }
    let _ = stream.shutdown().await;
    let drain = async {
        let mut buf = [0u8; 4096];
        let mut left: usize = 256 * 1024;
        while left > 0 {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => left = left.saturating_sub(n),
            }
        }
    };
    let _ = timeout(Duration::from_secs(1), drain).await;
}

/// Refuses at once, without waiting on the peer (`busy`): the `Error` if the socket takes it now.
pub fn refuse_now(stream: TcpStream, why: &str) {
    let message = format!("not allowed: {why}");
    if let Ok(b) = encode(&Frame::Error { message }) {
        let _ = stream.try_write(&b);
    }
}

/// Runs `conn` until it ends or `revoked` fires (then `conn` is dropped, closing its socket); the revocation's reason.
pub async fn until_revoked<F: std::future::Future>(
    conn: F,
    revoked: Option<Revoked>,
) -> Option<String> {
    let revoked = async {
        match revoked {
            Some(r) => r.wait().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = conn => None,
        why = revoked => Some(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A WhoIs with set answers, counting its calls.
    struct Stub {
        selves: Mutex<Vec<u64>>,
        nodes: Vec<tailscale::Known>,
        peer: Mutex<tailscale::Peer>,
        sleep: Duration,
        self_calls: AtomicUsize,
        whois_calls: AtomicUsize,
    }

    impl Stub {
        fn new(selves: &[u64], user: u64, tagged: bool) -> Stub {
            Stub {
                selves: Mutex::new(selves.to_vec()),
                nodes: vec![],
                peer: Mutex::new(tailscale::Peer { user, tagged }),
                sleep: Duration::ZERO,
                self_calls: AtomicUsize::new(0),
                whois_calls: AtomicUsize::new(0),
            }
        }
    }

    impl WhoIs for Stub {
        fn ipv4(&self) -> io::Result<Ipv4Addr> {
            Ok(Ipv4Addr::new(100, 100, 1, 1))
        }
        fn status(&self) -> io::Result<tailscale::Status> {
            self.self_calls.fetch_add(1, Ordering::SeqCst);
            let mut s = self.selves.lock().unwrap();
            // The answers in turn; the last one sticks.
            let self_user = if s.len() > 1 { s.remove(0) } else { s[0] };
            Ok(tailscale::Status {
                self_user,
                nodes: self.nodes.clone(),
            })
        }
        fn whois(&self, _peer: SocketAddr) -> io::Result<tailscale::Peer> {
            self.whois_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.sleep);
            Ok(*self.peer.lock().unwrap())
        }
    }

    fn tailnet_peer(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 101, 102, last)), 50000)
    }

    fn auth(stub: &Arc<Stub>) -> Arc<Auth> {
        Arc::new(Auth::new(None, Some(stub.clone() as Arc<dyn WhoIs>)))
    }

    /// The verdict without the revocation handle.
    fn v(r: Result<Allowed, String>) -> Result<(), String> {
        r.map(|_| ())
    }

    #[tokio::test]
    async fn same_user_untagged_allowed() {
        let s = Arc::new(Stub::new(&[42], 42, false));
        assert_eq!(v(auth(&s).tailnet(tailnet_peer(1)).await), Ok(()));
    }

    #[tokio::test]
    async fn other_user_refused() {
        let s = Arc::new(Stub::new(&[42], 43, false));
        let r = v(auth(&s).tailnet(tailnet_peer(1)).await);
        assert_eq!(r, Err("another tailnet user".to_string()));
    }

    #[tokio::test]
    async fn tagged_refused() {
        let s = Arc::new(Stub::new(&[42], 42, true));
        let r = v(auth(&s).tailnet(tailnet_peer(1)).await);
        assert_eq!(r, Err("a tagged node".to_string()));
    }

    #[tokio::test]
    async fn whois_timeout_refused() {
        let s = Arc::new(Stub {
            sleep: Duration::from_secs(3),
            ..Stub::new(&[42], 42, false)
        });
        let a = auth(&s);
        let t0 = Instant::now();
        let r = v(a.tailnet(tailnet_peer(1)).await);
        assert!(
            t0.elapsed() < Duration::from_millis(2200),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(r, Err("whois timed out".to_string()));
    }

    #[tokio::test]
    async fn no_cli_refused() {
        let a = Arc::new(Auth::new(None, None));
        let r = v(a.tailnet(tailnet_peer(1)).await);
        assert_eq!(r, Err("no tailscale CLI".to_string()));
    }

    /// A clock the test moves.
    fn mock_clock() -> (Arc<Mutex<Instant>>, Clock) {
        let now = Arc::new(Mutex::new(Instant::now()));
        let n2 = now.clone();
        (now, Arc::new(move || *n2.lock().unwrap()))
    }

    #[tokio::test]
    async fn decision_cached_60s() {
        let s = Arc::new(Stub::new(&[42], 42, true));
        let (now, clock) = mock_clock();
        let a = Arc::new(Auth::new(None, Some(s.clone() as Arc<dyn WhoIs>)).with_clock(clock));
        // A deny, kept 60 s.
        assert_eq!(
            v(a.tailnet(tailnet_peer(2)).await),
            Err("a tagged node".into())
        );
        *now.lock().unwrap() += Duration::from_secs(59);
        assert_eq!(
            v(a.tailnet(tailnet_peer(2)).await),
            Err("a tagged node".into())
        );
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 1, "cached");
        // Past 60 s it is asked again (and now allowed).
        *s.peer.lock().unwrap() = tailscale::Peer {
            user: 42,
            tagged: false,
        };
        *now.lock().unwrap() += Duration::from_secs(2);
        assert_eq!(v(a.tailnet(tailnet_peer(2)).await), Ok(()));
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 2);
        // An allow is kept alike: within 60 s, no call at all.
        *now.lock().unwrap() += Duration::from_secs(59);
        assert!(a.tailnet(tailnet_peer(2)).await.is_ok());
        assert_eq!(
            s.whois_calls.load(Ordering::SeqCst),
            2,
            "fresh: no re-check"
        );
    }

    #[tokio::test]
    async fn self_user_reread_once_on_mismatch() {
        // Read at start (41), then re-read once on the mismatch (42, which is the peer's).
        let s = Arc::new(Stub::new(&[41, 42], 42, false));
        let a = auth(&s);
        assert_eq!(s.self_calls.load(Ordering::SeqCst), 1);
        assert_eq!(v(a.tailnet(tailnet_peer(1)).await), Ok(()));
        assert_eq!(s.self_calls.load(Ordering::SeqCst), 2);
        // Matching now: no re-read.
        assert_eq!(v(a.tailnet(tailnet_peer(2)).await), Ok(()));
        assert_eq!(s.self_calls.load(Ordering::SeqCst), 2);
        // Another user: one re-read, then refused.
        *s.peer.lock().unwrap() = tailscale::Peer {
            user: 7,
            tagged: false,
        };
        assert_eq!(
            v(a.tailnet(tailnet_peer(3)).await),
            Err("another tailnet user".into())
        );
        assert_eq!(s.self_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_tailnet_non_loopback_refused() {
        let s = Arc::new(Stub::new(&[42], 42, false));
        let a = auth(&s);
        let lan = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 50000);
        assert_eq!(v(a.tailnet(lan).await), Err("not a tailnet address".into()));
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 0, "not asked");
    }

    fn known(ip: [u8; 4], user: u64, tagged: bool) -> tailscale::Known {
        tailscale::Known {
            is_self: false,
            os: "iOS".into(),
            ips: vec![IpAddr::from(ip), "fd7a:115c:a1e0::9".parse().unwrap()],
            user,
            tagged,
        }
    }

    #[tokio::test]
    async fn known_peer_allowed_without_a_call() {
        let s = Arc::new(Stub {
            nodes: vec![
                known([100, 101, 102, 1], 42, false),
                known([100, 101, 102, 2], 42, true),
                known([100, 101, 102, 3], 9, false),
            ],
            // Whatever WhoIs would say, it is not asked for a pre-allowed peer.
            ..Stub::new(&[42], 0, true)
        });
        let a = auth(&s);
        assert!(a.tailnet(tailnet_peer(1)).await.is_ok());
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 0);
        // The tagged node and the other user's are not pre-allowed: asked, and refused.
        assert!(a.tailnet(tailnet_peer(2)).await.is_err());
        assert!(a.tailnet(tailnet_peer(3)).await.is_err());
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 2);
    }

    /// A daemon's TCP side on 127.0.0.1, which stands in for the Tailscale address and counts as no machine's own, so
    /// a loopback connection is a tailnet peer here; on `clock`. Its address and the auth.
    async fn tailnet_server(
        s: &Arc<Stub>,
        clock: Option<Clock>,
    ) -> (String, Arc<Auth>, tempfile::TempDir) {
        let mut a = Auth::new(None, Some(s.clone() as Arc<dyn WhoIs>));
        if let Some(c) = clock {
            a = a.with_clock(c);
        }
        a.range_any = true;
        a.nothing_local = true;
        // Learnt again now that the stand-in address is not this machine's own.
        a.learn(&s.status().unwrap());
        let a = Arc::new(a);
        a.set_tailscale(Ipv4Addr::LOCALHOST);
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::new());
        let terms =
            Arc::new(crate::terms::Terms::new(dir.path().to_path_buf(), |_| false).unwrap());
        tokio::spawn(crate::serve::serve_tcp(
            l,
            a.clone(),
            store,
            "fixture-mac".into(),
            terms,
        ));
        (addr, a, dir)
    }

    async fn snapshot_of(c: &mut crate::client::Client) {
        match timeout(Duration::from_secs(5), c.next())
            .await
            .unwrap()
            .unwrap()
        {
            Frame::Snapshot { .. } => {}
            f => panic!("expected a Snapshot, got {f:?}"),
        }
    }

    /// Reads a board client until the server closes; false if it is still open after 3 s.
    async fn board_closed(c: &mut crate::client::Client) -> bool {
        timeout(Duration::from_secs(3), async {
            while c.next().await.is_ok() {}
        })
        .await
        .is_ok()
    }

    /// A kept allow whose background re-check now denies: the connection that set it off, and every connection the
    /// same IP opened before it (a Board and a Terminals one), are closed; the next connect asks on the connect path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revalidation_deny_closes() {
        let s = Arc::new(Stub {
            nodes: vec![known([127, 0, 0, 1], 42, false)],
            ..Stub::new(&[42], 42, false)
        });
        let (now, clock) = mock_clock();
        let (addr, _a, _dir) = tailnet_server(&s, Some(clock)).await;
        // Two connections while the allow is fresh: no WhoIs.
        let mut board = crate::client::Client::connect_tcp(&addr, None, None)
            .await
            .unwrap();
        snapshot_of(&mut board).await;
        let (mut tx, mut rx) = crate::client::term_connect_tcp(&addr, None).await.unwrap();
        tx.send(&Frame::TermOpen {
            r#ref: 1,
            target: fleet_model::types::AttachTarget::Claude {
                profile: "default".into(),
                job: "not a job".into(),
            },
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
        match timeout(Duration::from_secs(5), rx.next())
            .await
            .unwrap()
            .unwrap()
        {
            Frame::TermEnd { r#ref: 1, .. } => {}
            f => panic!("the Terminals connection is served: {f:?}"),
        }
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 0);

        // The node is tagged since; 61 s on, a third connection is let in at once and sets off the re-check.
        *s.peer.lock().unwrap() = tailscale::Peer {
            user: 42,
            tagged: true,
        };
        *now.lock().unwrap() += Duration::from_secs(61);
        let t0 = Instant::now();
        let mut third = crate::client::Client::connect_tcp(&addr, None, None)
            .await
            .unwrap();
        snapshot_of(&mut third).await;
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "not on the connect path"
        );

        assert!(board_closed(&mut third).await, "the third is closed");
        assert!(
            board_closed(&mut board).await,
            "the earlier Board is closed"
        );
        let closed = timeout(Duration::from_secs(3), async {
            while rx.next().await.is_ok() {}
        })
        .await
        .is_ok();
        assert!(closed, "the earlier Terminals connection is closed");
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 1);

        // The entry is gone: the next connect asks on the connect path, and is refused.
        let mut next = crate::client::Client::connect_tcp(&addr, None, None)
            .await
            .unwrap();
        match timeout(Duration::from_secs(5), next.next())
            .await
            .unwrap()
            .unwrap()
        {
            Frame::Error { message } => assert_eq!(message, "not allowed: a tagged node"),
            f => panic!("expected the refusal, got {f:?}"),
        }
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn revalidation_allow_refreshes() {
        let s = Arc::new(Stub {
            nodes: vec![known([100, 101, 102, 1], 42, false)],
            ..Stub::new(&[42], 42, false)
        });
        let (now, clock) = mock_clock();
        let a = Arc::new(Auth::new(None, Some(s.clone() as Arc<dyn WhoIs>)).with_clock(clock));
        *now.lock().unwrap() += Duration::from_secs(61);
        let r = a.tailnet(tailnet_peer(1)).await.unwrap();
        // Still allowed: no revocation.
        assert!(
            timeout(Duration::from_millis(300), r.revoked.unwrap().wait())
                .await
                .is_err()
        );
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 1);
        // Refreshed: the next one within 60 s asks nothing.
        assert!(a.tailnet(tailnet_peer(1)).await.is_ok());
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 1);
    }

    /// Ruling P5-R9: this machine's own node is never pre-allowed, and a peer from its address is never asked of WhoIs
    /// (it is local: the key).
    #[tokio::test]
    async fn self_is_never_a_tailnet_peer() {
        let mut me = known([100, 101, 102, 9], 42, false);
        me.is_self = true;
        let s = Arc::new(Stub {
            nodes: vec![me],
            ..Stub::new(&[42], 42, false)
        });
        let a = auth(&s);
        let st = s.status().unwrap();
        assert!(!pre_allowed(&st, &st.nodes[0]));
        assert_eq!(
            v(a.tailnet(tailnet_peer(9)).await),
            Err("a local peer needs the key".into())
        );
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 0);
    }

    /// A pre-allowed peer, end to end over a real listener (127.0.0.1 standing in for the Tailscale address): no WhoIs
    /// call, and the connect-to-`Snapshot` time (printed; the report quotes it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prewarmed_peer_connect_to_snapshot() {
        let s = Arc::new(Stub {
            nodes: vec![known([127, 0, 0, 1], 42, false)],
            ..Stub::new(&[42], 0, true)
        });
        let (addr, _a, _dir) = tailnet_server(&s, None).await;
        let mut times = vec![];
        for _ in 0..21 {
            let t0 = Instant::now();
            let mut c = crate::client::Client::connect_tcp(&addr, None, None)
                .await
                .unwrap();
            match timeout(Duration::from_secs(5), c.next())
                .await
                .unwrap()
                .unwrap()
            {
                Frame::Snapshot { .. } => {}
                f => panic!("expected a Snapshot, got {f:?}"),
            }
            times.push(t0.elapsed());
        }
        times.sort();
        let median = times[times.len() / 2];
        eprintln!(
            "prewarmed connect-to-Snapshot: median {median:?}, max {:?}",
            times[times.len() - 1]
        );
        assert_eq!(s.whois_calls.load(Ordering::SeqCst), 0);
        assert!(median < Duration::from_millis(20), "{median:?}");
    }

    #[test]
    fn at_most_16_in_auth() {
        let a = Arc::new(Auth::new(None, None));
        let held: Vec<_> = (0..16).map(|_| a.enter().unwrap()).collect();
        assert!(a.enter().is_none());
        drop(held);
        assert!(a.enter().is_some());
    }

    #[test]
    fn the_tailnet_range() {
        assert!(in_tailnet(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(in_tailnet(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!in_tailnet(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!in_tailnet(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(usable(Ipv4Addr::UNSPECIFIED, true).is_err());
        assert!(usable(Ipv4Addr::new(8, 8, 8, 8), true).is_err());
        assert!(usable(Ipv4Addr::LOCALHOST, false).is_err());
        assert!(usable(Ipv4Addr::LOCALHOST, true).is_ok());
    }

    #[test]
    fn proc_net_tcp_gives_the_peer_sockets_uid() {
        // The peer 127.0.0.1:50000 (C350) talking to 127.0.0.1:47470 (B96E): its own row, and the accepted socket's.
        let t = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:B96E 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11 1 0 100 0 0 10 0\n\
   1: 0100007F:C350 0100007F:B96E 01 00000000:00000000 00:00000000 00000000  1001        0 12 1 0 20 4 30 10 -1\n\
   2: 0100007F:B96E 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 13 1 0 20 4 30 10 -1\n";
        let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:47470".parse().unwrap();
        if cfg!(target_endian = "little") {
            assert_eq!(proc_tcp_uid(t, peer, local), Some(1001));
            let six = "  sl  local_address                         remote_address                        st\n\
   0: 0000000000000000FFFF00000100007F:C350 0000000000000000FFFF00000100007F:B96E 01 00000000:00000000 00:00000000 00000000  1002 0 14\n";
            assert_eq!(proc_tcp_uid(six, peer, local), Some(1002));
        }
        assert_eq!(
            proc_tcp_uid(t, "127.0.0.1:50001".parse().unwrap(), local),
            None
        );
    }
}
