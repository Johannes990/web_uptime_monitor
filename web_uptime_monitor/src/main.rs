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
use sqlx::postgres::any::AnyConnectionBackend;
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

#[derive(Serialize, sqlx:FromRow, Template)]
#[template(path = "index.html")]
struct WebsiteLogs {
    logs: Vec<WebsiteInfo>
}

#[derive(sqlx::FromRow, Serialize)]
pub struct WebsiteStats {
    time: DateTime<Utc>,
    uptime_pct: Option<i16>,
}

#[derive(Serialize, sqlx::FromRow, Template)]
#[template(path = "single_website.html")]
struct SingleWebsiteLogs {
    log: WebsiteInfo,
    incidents: Vec<Incident>,
    monthly_data: Vec<WebsiteStats>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct Incident {
    time: DateTime<Utc>,
    statis: i16,
}

/*
/ error handling
 */
enum ApiError {
    SQLError(sqlx::Error)
}

enum SplitBy {
    Hour,
    Day
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

/*
/ this function gathers data about our websites in the
/ database and saves the data about websites to the
/ logs table
 */
async fn check_websites(db: PgPool) {
    let mut interval = time::interval(Duration::from_secs(60));

    loop {
        interval.tick().await;

        let ctx = Client::new();
        let mut res = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites").fetch_all(&db);

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
async fn create_website(State(state): State<AppState>, Form(new_website): Form<Website>)
    -> Result<impl AxumIntoResponse, impl AxumIntoResponse> {
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
        .fetch_all(&state.db)
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

/*
/ function to get the daily stats of a website
/ that's in our database
 */
async fn get_daily_stats(alias: &str, db: &PgPool) -> Result<Vec<WebsiteStats>, ApiError> {
    let data = sqlx::query_as::<_, WebsiteStats>(
        r#"
        SELECT date_trunc('hour', created_at) AS time,
        CAST(COUNT(CASE WHEN status=200 THEN 1 END) * 100 / COUNT(*) AS int2) AS uptime_pct
        FROM logs
        LEFT JOIN websites ON websites.id = logs.website_id
        WHERE websites.alias = $1
        GROUP BY time
        ORDER BY time ASC
        LIMIT 24
        "#
    )
    .bind(alias)
    .fetch_all(db).await?;

    let no_of_splits = 24;
    let no_of_seconds = 3600;
    let data = fill_data_gaps(data, no_of_splits, SplitBy::Hour, no_of_seconds);

    Ok(data)
}

/*
/ this function is for returning the monthly
/ stats for a website that's in the database
 */
async fn get_monthly_stats(alias: &str, db: &PgPool) -> Result<Vec<WebsiteStats>, ApiError> {
    let data = sqlx::query_as::<_, WebsiteStats>(
        r#"
        SELECT date_trunc('day', created_at) AS time,
        CAST(COUNT(CASE WHEN status=200 THEN 1 END) * 100 / COUNT(*) AS int2) AS uptime_pct
        FROM logs
        LEFT JOIN websites ON websites.id=logs.website_id
        WHERE websites.alias=$1
        GROUP BY time
        ORDER BY time ASC
        LIMIT 30
        "#
    )
        .bind(alias)
        .fetch_all(db).await?;

    let no_of_splits = 30;
    let no_of_seconds = 86400;
    let data = fill_data_gaps(data, no_of_splits, SplitBy::Day, no_of_seconds);

    Ok(data)
}

/*
/ fill the data vector with default data
/ if not enough data retrieved
 */
fn fill_data_gaps(mut data: Vec<WebsiteStats>, splits: i32, format: SplitBy, no_of_seconds: i32)
    -> Vec<WebsiteStats> {
    // if the length of data is not as long as the number of required splits
    // then we fill in the gaps
    if (data.len() as i32) < splits {
        // for each split, format the time and check if the timestamp exists
        for i in 1..24 {
            let time = Utc::now() - chrono::Duration::seconds((no_of_seconds * i).into());
            let time = time
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap();

            let time = if matches!(format, SplitBy::Day) {
                time.with_hour(0).unwrap()
            } else {
                time
            };

            // if timestamp doesn't exist, push a timestamp woth None
            if !data.iter().any(|x| x.time == time) {
                data.push(WebsiteStats {
                    time,
                    uptime_pct: None,
                });
            }
        }

        // lastly, sort the data
        data.sort_by(|a, b| b.time.cmp(&a.time));
    }

    data
}

/*
/ this function returns a log of website data
/ for the website that matches the given alias
 */
async fn get_website_by_alias(State(state): State<AppState>, Path(alias): Path<String>)
    -> Result<impl AskamaIntoResponse, ApiError> {
    let website = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites WHERE alias = $1")
        .bind(&alias)
        .fetch_one(&state.db)
        .await?;

    let last_24_hours_data = get_daily_stats(&website.alias, &state.db).await?;
    let monthly_data = get_monthly_stats(&website.alias, &state.db).await?;

    let incidents = sqlx::query_as::<_, Incident>(
        "SELECT logs.created_at AS time,\
        logs.status FROM logs\
        LEFT JOIN websites ON websites.id=logs.website_id\
        WHERE websites.alias=$1 AND logs.status!=200",
    )
    .bind(&alias)
    .fetch_all(&state.db)
    .await?;

    let log = WebsiteInfo {
        url: website.url,
        alias,
        data: last_24_hours_data,
    };

    Ok(SingleWebsiteLogs {
        log,
        incidents,
        monthly_data,
    })
}

async fn delete_website(State(state): State<AppState>, Path(alias): Path<String>)
    -> Result<impl AskamaIntoResponse, ApiError> {
    let mut tx = state.db.begin().await?;

    if let Err(e) = sqlx::query("DELETE FROM logs WHERE website_alias=$1")
        .bind(&alias)
        .execute(&mut *tx)
        .await {
        tx.rollback().await?;
        return Err(ApiError::SQLError(e));
    };

    if let Err(e) = sqlx::query("DELETE FROM websites WHERE alias=$1")
        .bind(&alias)
        .execute(&mut *tx)
        .await {
        tx.rollback().await?;
        return Err(ApiError::SQLError(e));
    }

    tx.commit().await?;
    Ok(StatusCode::OK)
}

// assuming our style css lives at "templates/styles.css"
async fn styles() -> impl AxumIntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/css")
        .body(include_str!("../templates/styles.css").to_owned())
        .unwrap()
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
async fn main(#[shuttle_shared_db::Postgres] db: PgPool)
    -> shuttle_axum::ShuttleAxum {
    sqlx::migrate!().run(&db).await.expect("Migrations went wrong:(");

    let state = AppState::new(db);

    let router = Router::new().route("/", get(hello_world)).with_state(state);

    Ok(router.into())
}