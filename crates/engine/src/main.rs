use clap::Parser;
use fliks_engine::gateway::{self, RouteTable, Upstream};
use std::net::SocketAddr;

// glibc keeps the peaks of a many-threaded server resident; musl malloc is slow under contention.
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// Run the command after `--` as the host, restarting it when it exits.
    #[arg(long, requires = "cmd")]
    supervise: bool,
    /// Host to forward non-engine routes to (http://host[:port]); serves them on --public-listen.
    #[arg(long, value_parser = Upstream::parse)]
    upstream: Option<Upstream>,
    /// Remote engine for route-table engine rows (http://host[:port]); no local transcoding.
    #[arg(long, value_parser = Upstream::parse, requires = "upstream")]
    engine_upstream: Option<Upstream>,
    /// Public listener of the gateway (default 0.0.0.0:4848).
    #[arg(long, requires = "upstream")]
    public_listen: Option<SocketAddr>,
    /// JSON route table replacing the default (docs/spec/gateway.md).
    #[arg(long, requires = "upstream")]
    gateway_routes: Option<std::path::PathBuf>,
    #[arg(last = true)]
    cmd: Vec<String>,
}

fn load_table(cli: &Cli) -> RouteTable {
    match &cli.gateway_routes {
        Some(f) => std::fs::read_to_string(f)
            .map_err(|e| e.to_string())
            .and_then(|j| RouteTable::from_json(&j)),
        None => Ok(RouteTable::fliks()),
    }
    .unwrap_or_else(|e| panic!("gateway routes: {e}"))
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if cli.engine_upstream.is_some() {
        return run_remote_gateway(cli).await;
    }
    let token =
        std::env::var("FLIKS_ENGINE_TOKEN").unwrap_or_else(|_| fliks_engine::random_hex(32));
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    println!("FLIKS_ENGINE_LISTENING {addr}");
    let hw = fliks_engine::hw::Hw::new(fliks_engine::hw::ffmpeg_detector(
        fliks_engine::hw::ffmpeg_path(),
    ));
    hw.start();
    let rt = fliks_engine::runtime::Runtime::new(
        hw,
        fliks_engine::cache::Cache::from_env(),
        fliks_engine::hw::ffmpeg_path(),
    );
    rt.start();
    let server = axum::serve(listener, fliks_engine::router(token.clone(), rt.clone()));
    let mut host_env = vec![];
    if let Some(up) = &cli.upstream {
        let table = load_table(&cli);
        let public = cli.public_listen.unwrap_or(([0, 0, 0, 0], 4848).into());
        let public = tokio::net::TcpListener::bind(public)
            .await
            .expect("bind public");
        println!(
            "FLIKS_ENGINE_PUBLIC_LISTENING {}",
            public.local_addr().unwrap()
        );
        let app = gateway::router(
            table,
            up.clone(),
            Some(fliks_engine::serve::router(rt.clone())),
        );
        tokio::spawn(async move {
            axum::serve(
                public,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        host_env.push(("PORT", up.port().to_string()));
    }
    if cli.supervise {
        tokio::spawn(async move { server.await });
        let code =
            fliks_engine::supervise::run(&cli.cmd, &format!("http://{addr}"), &token, &host_env)
                .await;
        rt.shutdown().await;
        return std::process::ExitCode::from(code);
    }
    server
        .with_graceful_shutdown(fliks_engine::supervise::shutdown_signal())
        .await
        .unwrap();
    rt.shutdown().await;
    std::process::ExitCode::SUCCESS
}

async fn run_remote_gateway(cli: Cli) -> std::process::ExitCode {
    let token = match std::env::var("FLIKS_ENGINE_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("--engine-upstream requires FLIKS_ENGINE_TOKEN to be set in the environment");
            return std::process::ExitCode::FAILURE;
        }
    };
    let host = cli.upstream.as_ref().expect("clap requires upstream");
    let engine = cli.engine_upstream.as_ref().expect("checked above");
    let table = load_table(&cli);
    let public = cli.public_listen.unwrap_or(([0, 0, 0, 0], 4848).into());
    let public = tokio::net::TcpListener::bind(public)
        .await
        .expect("bind public");
    println!(
        "FLIKS_ENGINE_PUBLIC_LISTENING {}",
        public.local_addr().unwrap()
    );
    let app = gateway::router_remote(table, host.clone(), engine.clone());
    let gateway = axum::serve(
        public,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let host_env = [("PORT", host.port().to_string())];
    if cli.supervise {
        tokio::spawn(async move {
            gateway.await.ok();
        });
        let code = fliks_engine::supervise::run(&cli.cmd, engine.url(), &token, &host_env).await;
        return std::process::ExitCode::from(code);
    }
    gateway
        .with_graceful_shutdown(fliks_engine::supervise::shutdown_signal())
        .await
        .expect("gateway");
    std::process::ExitCode::SUCCESS
}
