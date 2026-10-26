mod assets;
mod config;
mod http;
mod proxy;
mod session;
mod sign;
mod themes;

use std::{net::Ipv4Addr, net::Ipv6Addr, net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use kubicle_mailbox::settings::Store;
use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpStream, signal::ctrl_c, signal::unix::SignalKind, signal::unix::signal};
use tracing::{error, info};

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--health") {
        return health().await;
    }
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let Err(e) = run().await else { return ExitCode::SUCCESS };
    error!("{e}");
    ExitCode::FAILURE
}

/// `kubicle-server --health`, Docker's healthcheck, which runs the binary itself as the image has no shell: a GET of
/// `/healthz` on the loopback address of `KUBICLE_LISTEN`'s family and port. Exits 0 on a 200 within 5 s, else 1 with
/// the reason on stderr, which Docker keeps in the container's health log.
async fn health() -> ExitCode {
    let asked = async {
        let listen = config::listen()?;
        let loopback = if listen.is_ipv4() { Ipv4Addr::LOCALHOST.into() } else { Ipv6Addr::LOCALHOST.into() };
        let mut tcp = TcpStream::connect(SocketAddr::new(loopback, listen.port())).await?;
        tcp.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
        let mut answer = Vec::new();
        tcp.read_to_end(&mut answer).await?;
        let status = answer.split(|&b| b == b'\r').next().unwrap_or_default();
        Ok::<_, Box<dyn std::error::Error>>(String::from_utf8_lossy(status).into_owned())
    };
    let failed = match tokio::time::timeout(Duration::from_secs(5), asked).await {
        Ok(Ok(status)) if status.starts_with("HTTP/1.1 200 ") => return ExitCode::SUCCESS,
        Ok(Ok(status)) => format!("answered {status:?}"),
        Ok(Err(e)) => e.to_string(),
        Err(_) => "no answer in 5 s".to_owned(),
    };
    eprintln!("/healthz: {failed}");
    ExitCode::FAILURE
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env()?;
    std::fs::create_dir_all(&config.data_dir).map_err(|e| format!("data directory {}: {e}", config.data_dir.display()))?;
    let app = Arc::new(http::App {
        build: assets::build(&config.web_dir),
        store: Store::open(&config.data_dir.join("settings.sqlite"))?,
        signer: sign::Signer::new(),
        proxy: proxy::Proxy::new(),
        renders: http::Renders::new(&config.data_dir)?,
        sessions: session::Sessions::default(),
        config,
    });

    tokio::spawn(http::learn_size_limit(Arc::clone(&app)));
    let sweeper = Arc::clone(&app);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(600));
        loop {
            tick.tick().await;
            sweeper.sessions.sweep();
        }
    });

    let listen = app.config.listen;
    let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| format!("listen on {listen}: {e}"))?;
    info!("listening on {listen}");
    let service = http::router(app).into_make_service_with_connect_info::<SocketAddr>();
    // `docker stop` sends SIGTERM, which PID 1 ignores without a handler: it then killed kubicle 10 s later, and each
    // message parsed then kept a crash record (http.rs, `Renders`). Stopped here, no parse starts and those running end.
    let mut term = signal(SignalKind::terminate())?;
    let stop = async move {
        tokio::select! { _ = ctrl_c() => {}, _ = term.recv() => {} }
    };
    axum::serve(listener, service).with_graceful_shutdown(stop).await?;
    Ok(())
}
