//! `koma serve`: the HTTP server. Stateless; all it does in Milestone 0 is
//! answer the health check.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::process::ExitCode;

use axum::Router;
use axum::http::header::CACHE_CONTROL;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;

use crate::{log, signal};

pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], "ok\n")
}

/// Serves on `listener` until `shutdown` resolves, then finishes open requests.
pub async fn serve<F>(listener: TcpListener, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown)
        .await
}

pub fn run(listen: SocketAddr) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error("serve: cannot start runtime", &[("error", &e.to_string())]);
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let listener = TcpListener::bind(listen).await?;
        log::info(
            "serve: listening",
            &[("addr", &listener.local_addr()?.to_string())],
        );
        serve(listener, signal::shutdown()).await
    });
    match result {
        Ok(()) => {
            log::info("serve: stopped", &[]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error(
                "serve: failed",
                &[("addr", &listen.to_string()), ("error", &e.to_string())],
            );
            ExitCode::FAILURE
        }
    }
}
