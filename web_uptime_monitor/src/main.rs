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
/ website info
 */
#[derive(Serialize, Validate)]
struct WebsiteInfo {
    #[validate(url)]
    url: String,
    alias: String,
    data: Vec<WebsiteStats>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct WebsiteStats {
    time: DateTime<Utc>,
    uptime_pct: Option<i16>,
}

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

/*
/ our backend, create an initial route to add a URL
/ to monitor. we use the Validate trait to
/ automatically return an error if validation fails
 */
async fn create_website(
    State(state): State<AppState>,
    Form(new_website): Form<Website>,
) -> Result<impl AxumIntoResponse, impl AxumIntoResponse> {
    if new_website.validate().is_err() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Validation error: is your website a reachable URL?",
        ));
    }

    sqlx::query("INSERT INTO websites (url, alias) VALUES ($1, $2)")
        .bind(new_website.url)
        .bind(new_website.alias)
        .execute(&state.db)
        .await
        .unwrap();

    Ok(Redirect::to("/"))
}

/*
/ get a list of all the websites we're tracking and add
/ them to a vector of website data. if there are no results
/ askama will handle that automatically for us
 */
async fn get_websites(State(state): State<AppState>) -> Result<impl AskamaIntoResponse, ApiError> {
    let websites = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites")
        .fetch_all((&state.db))
        .await?;

    let mut logs = Vec::new();

    for website in websites {
        let data = get_daily_stats(&website.alias, &state.db).await?;

        logs.push(WebsiteInfo {
            url: website.url,
            alias: website.alias,
            data,
        });
    }

    Ok(WebsiteLogs { logs })
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