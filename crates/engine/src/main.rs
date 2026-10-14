use clap::Parser;
use std::net::SocketAddr;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// Run the command after `--` as the host, restarting it when it exits.
    #[arg(long, requires = "cmd")]
    supervise: bool,
    #[arg(last = true)]
    cmd: Vec<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let token =
        std::env::var("FLIKS_ENGINE_TOKEN").unwrap_or_else(|_| fliks_engine::random_hex(32));
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    println!("FLIKS_ENGINE_LISTENING {addr}");
    let server = axum::serve(listener, fliks_engine::app(token.clone()));
    if cli.supervise {
        tokio::spawn(async move { server.await });
        let code = fliks_engine::supervise::run(&cli.cmd, &format!("http://{addr}"), &token).await;
        return std::process::ExitCode::from(code);
    }
    server
        .with_graceful_shutdown(fliks_engine::supervise::shutdown_signal())
        .await
        .unwrap();
    std::process::ExitCode::SUCCESS
}
