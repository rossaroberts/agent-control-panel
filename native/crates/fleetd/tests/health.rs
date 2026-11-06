//! The machine's health: the numbers against what the system's own tools print, no sampling while no Board client is
//! connected, and a change reaching a connected client.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fleet_model::wire::Frame;
use fleetd::client::Client;
use fleetd::health::{self, Options};
use fleetd::serve::{bind, serve};
use fleetd::store::Store;

fn tool(cmd: &str, args: &[&str]) -> String {
    let o = Command::new(cmd).args(args).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// User CPU plus system CPU of this process, in milliseconds.
fn cpu_ms() -> f64 {
    // SAFETY: all-zero is a valid `rusage`, and the call fills it.
    let u = unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        u
    };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    tv(u.ru_utime) + tv(u.ru_stime)
}

#[test]
fn the_values_are_percents_and_agree_with_the_system_tools() {
    let f = health::sample_fast();
    for v in [f.cpu_pct, f.mem_pct, f.disk_pct] {
        assert!(v <= 100, "{f:?}");
    }

    // Disk: `df -k $HOME` prints the capacity (on APFS its Used is the data volume's, ours the container's: a few
    // points apart, never below it).
    let home = std::env::var("HOME").unwrap();
    let df = tool("df", &["-k", &home]);
    let cap: i64 = df
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .find_map(|w| w.strip_suffix('%')?.parse().ok())
        .expect("df capacity");
    let gap = f.disk_pct as i64 - cap;
    assert!((-1..=5).contains(&gap), "df {cap} vs {f:?}");
    assert!(f.disk_pct > 0);

    // CPU: the load over the cores, as `sysctl vm.loadavg` and `hw.ncpu` say (load moves, so a margin).
    #[cfg(target_os = "macos")]
    {
        let la = tool("sysctl", &["-n", "vm.loadavg"]);
        let load: f64 = la
            .split_whitespace()
            .find_map(|w| w.parse().ok())
            .expect("load");
        let cpus: f64 = tool("sysctl", &["-n", "hw.ncpu"]).trim().parse().unwrap();
        let want = (100.0 * load / cpus).min(100.0);
        assert!(
            (want - f.cpu_pct as f64).abs() <= 25.0,
            "load {load} on {cpus}: want about {want}, got {f:?}"
        );

        // Memory: active + wired + compressor over the installed memory, as `vm_stat` counts them.
        let vm = tool("vm_stat", &[]);
        let page: f64 = vm
            .split("page size of ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let n = |k: &str| -> f64 {
            vm.lines()
                .find_map(|l| l.strip_prefix(k)?.trim().trim_end_matches('.').parse().ok())
                .unwrap_or_else(|| panic!("vm_stat: {k}"))
        };
        let used =
            (n("Pages active:") + n("Pages wired down:") + n("Pages occupied by compressor:"))
                * page;
        let total: f64 = tool("sysctl", &["-n", "hw.memsize"])
            .trim()
            .parse()
            .unwrap();
        let want = 100.0 * used / total;
        assert!(
            (want - f.mem_pct as f64).abs() <= 5.0,
            "vm_stat says {want:.1}%, got {f:?}"
        );
    }
    assert!(f.mem_pct > 0);
}

#[test]
fn the_subprocess_answers_are_read_as_node_reads_them() {
    // lsof -Fpcn: node on 3000 and again on 3000 (one port), vite on 5173, a Python server, the daemon's own pid.
    let lsof = "p100\ncnode\nn*:3000\nn[::1]:3000\np101\ncvite\nn127.0.0.1:5173\np102\ncpython3\nn*:8000\np4242\ncnode\nn*:4747\n";
    assert_eq!(health::count_lsof_dev(lsof, 4242), 2);
    assert_eq!(health::count_lsof_dev(lsof, 0), 3);
    assert_eq!(health::count_lsof_dev("", 0), 0);

    let ss = "LISTEN 0 511 0.0.0.0:3000 0.0.0.0:* users:((\"node\",pid=100,fd=19))\n\
              LISTEN 0 128 [::]:5173 [::]:* users:((\"vite\",pid=101,fd=20))\n\
              LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=1,fd=3))\n\
              LISTEN 0 128 0.0.0.0:9000 0.0.0.0:*\n";
    assert_eq!(health::count_ss_dev(ss, 4242), 2);

    let docker = "supabase_db_app\nsupabase_kong_app\nsupabase_db_other\nredis\nsupabase_db_\n";
    assert_eq!(health::count_db_stacks(docker), 2);
}

#[test]
fn the_client_count_follows_the_connections() {
    let store = Arc::new(Store::new());
    let rx = store.subscribe_clients();
    assert_eq!(*rx.borrow(), 0);
    let a = store.board_client();
    let b = store.board_client();
    assert_eq!(*rx.borrow(), 2);
    drop(a);
    assert_eq!(*rx.borrow(), 1);
    drop(b);
    assert_eq!(*rx.borrow(), 0);
}

/// Nothing is sampled, and next to no CPU is used, over 30 s with no client; a client starts it and its leaving stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_sampling_without_a_client() {
    let store = Arc::new(Store::new());
    let task = health::spawn(store.clone(), Options::default());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (c0, t0) = (cpu_ms(), Instant::now());
    tokio::time::sleep(Duration::from_secs(30)).await;
    let used = cpu_ms() - c0;
    eprintln!("idle: {used:.1} ms of CPU in {:?}", t0.elapsed());
    assert_eq!(task.samples(), 0);
    assert_eq!(task.slow_runs(), 0);
    assert_eq!(store.meta().health, None);
    // The idle budget (Ruling P2-R6): 50 ms per 60 s, so 25 ms for this half.
    assert!(used <= 25.0, "{used:.1} ms of CPU with no client");

    let guard = store.board_client();
    let t = Instant::now();
    while task.samples() == 0 {
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "no sample for a client"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    while store.meta().health.is_none() {
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "no health for a client"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(guard);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let n = task.samples();
    // With a 5 s tick, a sample after the client left would come within this; none does.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(task.samples(), n, "sampled with no client");
}

/// The processes are asked at most once per `slow_every`, however often the numbers are read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_subprocesses_run_at_most_once_per_interval() {
    let store = Arc::new(Store::new());
    let task = health::spawn(
        store.clone(),
        Options {
            every: Duration::from_millis(100),
            slow_every: Duration::from_millis(1500),
        },
    );
    let _c = store.board_client();
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let (s, r) = (task.samples(), task.slow_runs());
    assert!(s >= 20, "{s} samples");
    assert!(
        (2..=3).contains(&r),
        "{r} subprocess runs in 3.3 s at 1.5 s"
    );
}

/// A Board client is sent the health once it is read, and again when it changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connected_client_is_sent_the_health_within_six_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleetd.sock");
    let store = Arc::new(Store::new());
    let (listener, _guard) = bind(&path).unwrap();
    let terms = Arc::new(fleetd::terms::Terms::new(dir.path().to_path_buf(), |_| false).unwrap());
    let server = tokio::spawn(serve(listener, store.clone(), "fixture-mac".into(), terms));
    let _health = health::spawn(store.clone(), Options::default());

    let mut c = Client::connect(&path, None).await.unwrap();
    let t = Instant::now();
    let got = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if let Frame::Meta(m) = c.next().await.unwrap() {
                if let Some(h) = m.health {
                    return h;
                }
            }
        }
    })
    .await
    .expect("a Meta with health within 6 s");
    eprintln!("health after {:?}: {got:?}", t.elapsed());
    assert!(got.mem_pct > 0 && got.disk_pct > 0 && got.cpu_pct <= 100);

    // A change in the store reaches the client as the next Meta.
    store.modify_meta(|m| {
        m.health.as_mut().unwrap().dev = 99;
        true
    });
    let again = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if let Frame::Meta(m) = c.next().await.unwrap() {
                if m.health.is_some_and(|h| h.dev == 99) {
                    return;
                }
            }
        }
    })
    .await;
    assert!(again.is_ok(), "the changed health is sent");
    server.abort();
}
