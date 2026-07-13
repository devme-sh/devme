//! Per-instance supervisor daemon binary.

fn main() {
    if let Err(error) = devme_supervisor::runtime::run() {
        eprintln!("devme-supervisor: {error}");
        std::process::exit(1);
    }
}
