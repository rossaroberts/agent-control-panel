//! fleetd: `serve`, `snapshot [--json]`, `watch`, `roster-parity`, `install-service`, `uninstall-service`.

mod parity;

use std::process::exit;
use std::sync::Arc;

use fleet_model::wire::Frame;
use fleet_read::home::home_dir;
use fleetd::store::Store;
use fleetd::{bench, client, machine, out, serve, service, watch};

const USAGE: &str = "usage: fleetd serve | snapshot [--json] | watch | install-service | uninstall-service\n       fleetd roster-parity [--rounds N] [--interval SECS] [--out FILE]";

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
        Some("serve") => run_serve(),
        Some("snapshot") => run_snapshot(args.iter().any(|a| a == "--json")),
        Some("watch") => run_watch(),
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

fn run_serve() {
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
    let handle = watch::watch(&home, store.clone()).unwrap_or_else(|e| {
        eprintln!("fleetd: watch: {e}");
        exit(1)
    });
    let _guard = rt.block_on(async {
        eprintln!(
            "fleetd: serving {} (run {:016x})",
            path.display(),
            store.run()
        );
        let machine = machine::machine_name(&home);
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = serve::serve(listener, store, machine) => {}
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        guard // kept to the end of `serve`: the socket goes, then the lock
    });
    drop(rt);
    drop(handle);
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

fn run_watch() {
    let path = serve::socket_path();
    let r: std::io::Result<()> = runtime().block_on(async {
        let mut c = client::Client::connect(&path, None).await?;
        loop {
            match c.next().await? {
                Frame::Snapshot {
                    run,
                    seq,
                    machine,
                    sessions,
                } => println!("{}", out::snapshot(run, seq, &machine, &sessions)),
                Frame::Delta(d) => println!("{}", out::delta(&d)),
                Frame::Error { message } => return Err(std::io::Error::other(message)),
                _ => {}
            }
        }
    });
    if let Err(e) = r {
        eprintln!("fleetd: watch {}: {e}", path.display());
        exit(1);
    }
}
