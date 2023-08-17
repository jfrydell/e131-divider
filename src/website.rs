use std::sync::Arc;

use axum::{
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use tokio::sync::Mutex;

use crate::State as AppState;

pub async fn main(state: Arc<Mutex<AppState>>) {
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

async fn get_state(State(state): State<Arc<Mutex<AppState>>>) -> Json<AppState> {
    Json(state.lock().await.clone())
}
