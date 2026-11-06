//! fleetd: `serve`, `snapshot [--json]`, `watch`, `roster-parity`, `install-service`, `uninstall-service`.

mod parity;

use std::process::exit;
use std::sync::Arc;

use fleet_model::wire::Frame;
use fleet_read::home::home_dir;
use fleetd::store::Store;
use fleetd::{
    attach, bench, client, config, health, machine, net, out, serve, service, tailscale, terms,
    watch,
};

const USAGE: &str = "usage: fleetd serve [--listen off|loopback|tailscale] [--port N] [--config FILE] | snapshot [--json] | watch [--tcp HOST:PORT] | tailnet | attach {--list|<profile>/<job>|codex/<id>|--trust <profile> <dir>} | install-service | uninstall-service\n       fleetd roster-parity [--rounds N] [--interval SECS] [--out FILE]";

fn main() {
    // A panic on any thread ends the process: every shared structure is behind a std Mutex, so a panicking watcher
    // thread would poison the board and leave a daemon that still serves a frozen one, which launchd's KeepAlive never
    // restarts. Release builds abort (`panic = "abort"`); this covers debug builds and says what happened first.
    std::panic::set_hook(Box::new(|info| {
        let t = std::thread::current();
        eprintln!(
            "fleetd: panic on thread {}: {info}",
            t.name().unwrap_or("?")
        );
        exit(1);
    }));
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("roster-parity") => parity::run(&args[1..]),
        Some("serve") => run_serve(&args[1..]),
        Some("snapshot") => run_snapshot(args.iter().any(|a| a == "--json")),
        Some("watch") => run_watch(&args[1..]),
        Some("tailnet") => run_tailnet(),
        Some("attach") => exit(attach::run(&args[1..])),
        Some("bench") => done(runtime().block_on(bench::run(&args[1..]))),
        Some("install-service") => done(service::install()),
        Some("uninstall-service") => done(service::uninstall()),
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}

fn done(r: std::io::Result<()>) {
    if let Err(e) = r {
        eprintln!("fleetd: {e}");
        exit(1);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime")
}

/// `serve`'s flags over the config file's: the listener and its port.
fn serve_config(args: &[String]) -> (config::Config, std::path::PathBuf) {
    let fail = |m: String| -> ! {
        eprintln!("fleetd: {m}\n{USAGE}");
        exit(2)
    };
    let (mut listen, mut port, mut file) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .unwrap_or_else(|| fail(format!("{a} needs a value")))
        };
        match a.as_str() {
            "--listen" => listen = Some(val().parse().unwrap_or_else(|e: String| fail(e))),
            "--port" => {
                let v = val();
                port = Some(
                    v.parse::<u16>()
                        .unwrap_or_else(|_| fail(format!("--port {v}"))),
                )
            }
            "--config" => file = Some(std::path::PathBuf::from(val())),
            _ => fail(format!("serve: unknown argument {a}")),
        }
    }
    let mut cfg = config::load(file.as_deref()).unwrap_or_else(|e| {
        eprintln!("fleetd: config: {e}");
        exit(1)
    });
    if let Some(l) = listen {
        cfg.listen = l;
    }
    if let Some(p) = port {
        cfg.port = p;
    }
    (cfg, config::dir_of(file.as_deref()))
}

fn run_serve(args: &[String]) {
    let (cfg, cfg_dir) = serve_config(args);
    let home = home_dir();
    let path = serve::socket_path();
    let rt = runtime();
    // The lock and the socket first: a second daemon stops here, before any startup read. A client that connects
    // meanwhile waits in the backlog for the first full read.
    let (listener, guard) = {
        let _in = rt.enter();
        serve::bind(&path).unwrap_or_else(|e| {
            eprintln!("fleetd: {}: {e}", path.display());
            exit(1)
        })
    };
    let store = Arc::new(Store::new());
    let handle = Arc::new(watch::watch(&home, store.clone()).unwrap_or_else(|e| {
        eprintln!("fleetd: watch: {e}");
        exit(1)
    }));
    // The terminals ask the board whether a running Codex holds a thread (lock order: terminals, then board), and for a
    // process scan once a Codex terminal has gone (asked with no terminals' lock held).
    let (held, req, wait) = (handle.clone(), handle.clone(), handle.clone());
    let rescan = terms::Rescan {
        request: Box::new(move || req.request_scan()),
        wait: Box::new(move |ticket, d| wait.wait_scan(ticket, d)),
    };
    let terms = Arc::new(
        terms::Terms::with_rescan(
            home.clone(),
            move |sid| held.codex_held(sid),
            terms_options(),
            Some(rescan),
        )
        .unwrap_or_else(|e| {
            eprintln!("fleetd: terminals: {e}");
            exit(1)
        }),
    );
    // The TCP listener, only when asked (off by default): the key beside the config, then the binds.
    let auth = (cfg.listen != config::Listen::Off).then(|| {
        let key = config::key(&cfg_dir).unwrap_or_else(|e| {
            eprintln!("fleetd: key: {e}");
            exit(1)
        });
        let ts = (cfg.listen == config::Listen::Tailscale)
            .then(tailscale::Cli::find)
            .flatten()
            .map(|c| Arc::new(c) as Arc<dyn net::WhoIs>);
        Arc::new(net::Auth::new(Some(key), ts))
    });
    let tcp = auth.as_ref().map(|a| {
        let _in = rt.enter();
        net::listen(&cfg, a.clone()).unwrap_or_else(|e| {
            eprintln!("fleetd: {e}");
            exit(1)
        })
    });
    let _guard = rt.block_on(async {
        eprintln!(
            "fleetd: serving {} (run {:016x})",
            path.display(),
            store.run()
        );
        let machine = machine::machine_name(&home);
        if let (Some(auth), Some(listeners)) = (&auth, tcp) {
            let port = listeners
                .first()
                .and_then(|l| l.local_addr().ok())
                .map_or(cfg.port, |a| a.port());
            for l in listeners {
                let (a, s, m, t) = (auth.clone(), store.clone(), machine.clone(), terms.clone());
                tokio::spawn(serve::serve_tcp(l, a, s, m, t));
            }
            if cfg.listen == config::Listen::Tailscale && auth.tailscale_addr().is_none() {
                let (a, s, m, t) = (auth.clone(), store.clone(), machine.clone(), terms.clone());
                tokio::spawn(async move {
                    if let Some(l) = net::tailscale_later(port, a.clone()).await {
                        serve::serve_tcp(l, a, s, m, t).await;
                    }
                });
            }
        }
        // The machine's health is sampled while a Board client is connected, and not otherwise.
        let _health = health::spawn(store.clone(), health::Options::default());
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = serve::serve(listener, store, machine, terms) => {}
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        guard // kept to the end of `serve`: the socket goes, then the lock
    });
    drop(rt);
    drop(handle);
}

/// The terminals' tunables: the defaults, but debug builds take `FLEETD_TEST_WARM_TTL_MS` for the warm TTL (a live
/// check of what a warm expiry does to a real session, without waiting 10 minutes).
fn terms_options() -> terms::Options {
    #[allow(unused_mut)]
    let mut o = terms::Options::default();
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("FLEETD_TEST_WARM_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        o.warm_ttl = std::time::Duration::from_millis(ms);
    }
    o
}

/// One read, in this process, with no daemon: the watcher's startup read, then print.
fn run_snapshot(json: bool) {
    let home = home_dir();
    let store = Arc::new(Store::new());
    let handle = watch::watch(&home, store.clone()).unwrap_or_else(|e| {
        eprintln!("fleetd: watch: {e}");
        exit(1)
    });
    let (seq, sessions) = store.snapshot();
    drop(handle);
    let machine = machine::machine_name(&home);
    if json {
        println!("{}", out::snapshot(store.run(), seq, &machine, &sessions));
    } else {
        println!("{} sessions on {machine} (seq {seq})", sessions.len());
        for s in &sessions {
            println!(
                "{:<11} {:<6} {}  {}",
                out::state_word(&s.state),
                out::engine_word(&s.engine),
                s.name.as_deref().unwrap_or(&s.key.session_id),
                s.reason
            );
        }
    }
}

/// The tailnet nodes `tailscale status` lists, and whether a `--listen tailscale` daemon pre-allows each (Ruling P5-R7).
fn run_tailnet() {
    let Some(cli) = tailscale::Cli::find() else {
        eprintln!("fleetd: no tailscale CLI");
        exit(1)
    };
    let s = cli.status().unwrap_or_else(|e| {
        eprintln!("fleetd: tailscale status: {e}");
        exit(1)
    });
    println!("self user {}", s.self_user);
    for (n, allowed) in net::Auth::listing(&s) {
        let ips: Vec<String> = n
            .ips
            .iter()
            .filter(|i| i.is_ipv4())
            .map(|i| i.to_string())
            .collect();
        let why = if allowed {
            "allowed"
        } else if n.tagged {
            "refused (tagged)"
        } else {
            "refused (another user)"
        };
        println!("{:<8} {:<16} {why}", n.os, ips.join(","));
    }
}

fn run_watch(args: &[String]) {
    let tcp = match args {
        [] => None,
        [f, a] if f == "--tcp" => Some(a.clone()),
        _ => {
            eprintln!("{USAGE}");
            exit(2)
        }
    };
    let path = serve::socket_path();
    let r: std::io::Result<()> = runtime().block_on(async {
        let mut c = match &tcp {
            Some(a) => client::Client::connect_tcp(a, None, None).await?,
            None => client::Client::connect(&path, None).await?,
        };
        loop {
            match c.next().await? {
                Frame::Snapshot {
                    run,
                    seq,
                    machine,
                    sessions,
                } => println!("{}", out::snapshot(run, seq, &machine, &sessions)),
                Frame::Delta(d) => println!("{}", out::delta(&d)),
                Frame::Meta(m) => println!("{}", out::meta(&m)),
                Frame::Error { message } => return Err(std::io::Error::other(message)),
                _ => {}
            }
        }
    });
    if let Err(e) = r {
        let to = tcp.unwrap_or_else(|| path.display().to_string());
        eprintln!("fleetd: watch {to}: {e}");
        exit(1);
    }
}
