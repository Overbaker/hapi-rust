fn main() -> std::io::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()?
        .block_on(async {
            let args: Vec<String> = std::env::args().skip(1).collect();
            if args.first().is_some_and(|s| s == "hook-forwarder") {
                hapi_wrapper_core::hooks::forwarder::run(&args[1..]).await?;
            }
            Ok(())
        })
}
