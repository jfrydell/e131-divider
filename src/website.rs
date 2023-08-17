use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use tokio::sync::RwLock;

pub async fn main(state: Arc<RwLock<SiteState>>) {
    let router = Router::new()
        .route("/", get(status_page))
        .route("/state", get(get_state))
        .with_state(state);
    axum::Server::bind(&"0.0.0.0:80".parse().unwrap())
        .serve(router.into_make_service())
        .await
        .unwrap()
}
async fn status_page() -> impl IntoResponse {
    Html("<html><head><title>Lighting Controller</title></head><body><h1>Lighting Controller</h1><p>Running</p></body></html>")
}

async fn get_state(State(state): State<Arc<RwLock<SiteState>>>) -> Json<SiteState> {
    Json(state.read().await.clone())
}

#[derive(Serialize, Clone)]
struct QueuedSource {
    addr: String,
    name: String,
    /// Frames since last output
    since_last: usize,
}
impl QueuedSource {
    fn new(addr: &SocketAddr, source_state: &crate::SourceState, frame: usize) -> Self {
        Self {
            addr: addr.to_string(),
            name: source_state.name.clone(),
            since_last: frame - source_state.last_packet,
        }
    }
}
#[derive(Serialize, Clone)]
struct OutputtingSource {
    addr: String,
    name: String,
    since_last: usize,
    output_time: usize,
    data: Vec<u8>,
}
impl OutputtingSource {
    fn new(addr: &SocketAddr, source_state: &crate::SourceState, frame: usize) -> Self {
        let (start_frame, data) = match &source_state.position {
            crate::SourcePosition::Outputting {
                start_frame,
                current_data,
                ..
            } => (start_frame, current_data),
            crate::SourcePosition::Queue => panic!("Source is not outputting"),
        };
        Self {
            addr: addr.to_string(),
            name: source_state.name.clone(),
            since_last: frame - source_state.last_packet,
            output_time: frame - start_frame,
            data: data.clone(),
        }
    }
}
/// Represents the website's state. This should be updated with `update()` every frame.
#[derive(Serialize, Clone)]
pub struct SiteState {
    queued: Vec<QueuedSource>,
    outputting: Vec<OutputtingSource>,
    frame: usize,
}
impl SiteState {
    pub fn new() -> Self {
        Self {
            queued: Vec::new(),
            outputting: Vec::new(),
            frame: 0,
        }
    }
    /// Updates the site state given exclusive access to the app state (for immediate access to sources).
    pub fn update(state: &mut crate::State) -> Self {
        let queued: Vec<QueuedSource> = state
            .queue
            .iter()
            .map(|addr| {
                QueuedSource::new(
                    addr,
                    state.sources.get_mut(addr).unwrap().get_mut().unwrap(),
                    state.frame,
                )
            })
            .collect();
        let outputting: Vec<OutputtingSource> = state
            .outputters
            .iter()
            .map(|addr| {
                OutputtingSource::new(
                    addr,
                    state.sources.get_mut(addr).unwrap().get_mut().unwrap(),
                    state.frame,
                )
            })
            .collect();
        Self {
            queued,
            outputting,
            frame: state.frame,
        }
    }
}
