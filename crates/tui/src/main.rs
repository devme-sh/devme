fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("devme: tokio init failed: {e}");
            std::process::exit(1);
        }
    };
    let no_shutdown = std::env::args().any(|a| a == "--no-shutdown");
    let exit_code = match runtime.block_on(devme_tui::launch(no_shutdown)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("devme: {e}");
            1
        }
    };
    std::process::exit(exit_code);
}
