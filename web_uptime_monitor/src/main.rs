use askama::Template;
use chrono::Timelike;
use askama_axum::IntoResponse as AskamaIntoResponse;
use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{IntoResponse as AxumIntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::time::{self, Duration};
use validator::Validate;


async fn hello_world() {
    println!("Hello, world!")
}

#[derive(Clone)]
struct AppState {
    db: PgPool,
}

impl AppState {
    fn new(db: PgPool) -> Self {
        Self {db}
    }
}

#[shuttle_runtime::main]
async fn main(
    #[shuttle_shared_db::Postgres] db: PgPool
) -> shuttle_axum::ShuttleAxum {
    sqlx::migrate!().run(&db).await.expect("Migrations went wrong:(");

    let state = AppState::new(db);

    let router = Router::new().route("/", get(hello_world)).with_state(state);

    Ok(router.into())
}