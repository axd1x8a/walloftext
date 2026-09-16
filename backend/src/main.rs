mod auth;
mod error;
mod network;
mod persistence;
mod preview;
mod revert;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;
use walloftext_shared::FontAtlasFile;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let atlas_bytes = std::fs::read("static/unifont.wtfont")
        .map_err(|e| anyhow::anyhow!("failed to read unifont.wtfont: {e}"))?;
    let atlas_bytes = zstd::decode_all(atlas_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("failed to decompress font atlas: {e}"))?;
    let font_atlas: FontAtlasFile = bitcode::decode(&atlas_bytes)
        .map_err(|e| anyhow::anyhow!("failed to decode font atlas: {e}"))?;
    let font_atlas = Arc::new(font_atlas);

    let index_html = Arc::new(
        std::fs::read_to_string("static/index.html")
            .map_err(|e| anyhow::anyhow!("failed to read index.html: {e}"))?,
    );

    let admin_token = std::env::var("ADMIN_TOKEN")
        .map_err(|_| anyhow::anyhow!("ADMIN_TOKEN env var must be set"))?;

    let (segment_tx, segment_rx) = mpsc::unbounded_channel();

    let state = state::AppState::new(segment_tx, font_atlas, index_html, admin_token);
    let last_segment_id = state.hydrate().await?;
    persistence::start_segment_worker(
        segment_rx,
        last_segment_id,
        state.inner.last_flushed_segment_id.clone(),
        state.inner.force_segment_flush.clone(),
    );
    state.start_snapshot_worker();
    state.start_cell_update_worker();

    let force_flush_on_shutdown = state.inner.force_segment_flush.clone();
    let app = network::build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("Listening on {}", addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app)
        .with_graceful_shutdown(shutdown_signal(force_flush_on_shutdown))
        .await?;
    Ok(())
}

async fn shutdown_signal(force_flush: Arc<std::sync::atomic::AtomicBool>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received, flushing pending segment data...");
    force_flush.store(true, std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}
