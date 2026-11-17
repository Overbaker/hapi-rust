use hapi_hub_rs::{build_router, HubState};
use std::env;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let listen = env::var("HAPI_HUB_RS_LISTEN").unwrap_or_else(|_| "127.0.0.1:18787".to_string());
    let db_path = env::var("HAPI_HUB_RS_DB").unwrap_or_else(|_| ":memory:".to_string());

    let state = HubState::open(&db_path).expect("failed to initialize hapi-hub-rs database");
    let listener = TcpListener::bind(&listen)
        .await
        .expect("failed to bind HAPI_HUB_RS_LISTEN");

    println!("hapi-hub-rs listening on http://{listen} (db={db_path})");
    axum::serve(listener, build_router(state))
        .await
        .expect("axum server exited with error");
}
