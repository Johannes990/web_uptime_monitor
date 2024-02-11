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

/*
/ error handling
 */
enum ApiError {
    SQL(sqlx::Error)
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        Self::SQLError(e)
    }
}

impl AxumIntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::SQLError(e) => {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("SQL Error: {e}")
                    ).into_response()
            }
        }
    }
}


/*
/ monitoring is done by fetching a list of websites from
/ the database and sequentially sending HTTP requests to
/ them and recording results in postgres
 */

#[derive(Deserialize, sqlx::FromRow, Validate)]
struct Website {
    #[validate(url)]
    url: String,
    alias: String
}

async fn check_websites(db: PgPool) {
    let mut interval = time::interval(Duration::from_secs(60));

    loop {
        interval.tick().await;

        let ctx = Client::new();
        let mut res = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites").fetch(&db);

        while let Some(website) = res.next().await {
            let website = website.unwrap();
            let response = ctx.get(website.url).send().await.unwrap();

            sqlx::query(
                "INSERT INTO logs (website_alias, status)\
                VALUES\
                ((SELECT id FROM websites where alias = $1), $2)"
            )
                .bind(website.alias)
                .bind(response.status().as_u16() as i16)
                .execute(&db).await
                .unwrap();
        }
    }

}

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