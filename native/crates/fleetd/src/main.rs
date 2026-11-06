//! fleetd. Subcommands are added here as they land; today: `roster-parity`.

mod parity;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("roster-parity") => parity::run(&args[1..]),
        _ => {
            eprintln!("usage: fleetd roster-parity [--rounds N] [--interval SECS] [--out FILE]");
            std::process::exit(2);
        }
    }
}
