//! A small JSON API on axum reporting to Fixwire: each request gets its own
//! scope and a trace named after its route, a panic is reported as a crash
//! (and answered 500), a failed payment is reported with the order as
//! context, the call to the inventory service is traced, and log records
//! become breadcrumbs.
//!
//! ```sh
//! FIXWIRE_DSN=https://<key>@<host> cargo run --bin shop-api
//! ```

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fixwire::{Options, OutgoingRequest, User};
use serde::Deserialize;
use serde_json::{Map, json};
use tracing_subscriber::prelude::*;

/// What the payment provider answers with.
#[derive(Debug)]
struct PaymentDeclined {
    code: &'static str,
}

impl fmt::Display for PaymentDeclined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "payment declined: {}", self.code)
    }
}

impl Error for PaymentDeclined {}

/// Charging an order failed: the order, and why.
#[derive(Debug)]
struct ChargeFailed {
    order: String,
    cause: PaymentDeclined,
}

impl fmt::Display for ChargeFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "charging order {}", self.order)
    }
}

impl Error for ChargeFailed {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}

/// The inventory service would not hold the item.
#[derive(Debug)]
struct ReserveFailed(String);

impl fmt::Display for ReserveFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ReserveFailed {}

struct App {
    http: reqwest::Client,
    inventory_url: String,
}

#[tokio::main]
async fn main() {
    let inventory_url = env("INVENTORY_URL", "http://localhost:8081");
    // The DSN comes from FIXWIRE_DSN; without it, Fixwire does nothing. Keep the guard: when main
    // returns, it sends what is left.
    let _fixwire = fixwire::init(Options {
        release: Some(env("RELEASE", "shop-api@1.0.0")),
        traces_sample_rate: 1.0,
        // Trace headers go to our own inventory service, nowhere else.
        trace_propagation_targets: vec![inventory_url.clone()],
        ..Options::default()
    });
    // Log records become breadcrumbs (and Fixwire events from error up).
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(fixwire::tracing::layer())
        .init();

    let app = Arc::new(App {
        http: reqwest::Client::new(),
        inventory_url,
    });
    let router = Router::new()
        .route("/products/{id}", get(product))
        .route("/orders", post(create_order))
        .route("/admin/report", get(report))
        .with_state(app)
        .layer(middleware::from_fn(with_user))
        // Fixwire outside the routes, so requests are named after them; the panic layer outside
        // Fixwire answers 500 to what panicked, once Fixwire has reported it.
        .layer(fixwire::tower::FixwireLayer::new())
        .layer(tower_http::catch_panic::CatchPanicLayer::new());

    let listener =
        tokio::net::TcpListener::bind(("0.0.0.0", env("PORT", "8080").parse().unwrap_or(8080)))
            .await
            .expect("a port to listen on");
    println!(
        "listening on {}",
        listener.local_addr().expect("an address").port()
    );
    // On SIGTERM (or Ctrl-C): finish the requests under way; then the guard sends what is left.
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await
        .expect("serving");
}

/// The signed-in user (here: a header), on the request's scope.
async fn with_user(request: Request, next: Next) -> Response {
    if let Some(id) = request
        .headers()
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
    {
        fixwire::set_user(Some(User::with_id(id)));
    }
    next.run(request).await
}

async fn product(Path(id): Path<String>) -> Response {
    // A span for the lookup, under the request's.
    let found = fixwire::trace("SELECT products", "db.query", |span| {
        span.set_attribute("db.system.name", "postgresql");
        match id.as_str() {
            "sku_1" => Some(json!({"id": "sku_1", "name": "Mug", "price_cents": 1200})),
            "sku_2" => Some(json!({"id": "sku_2", "name": "Poster", "price_cents": 2500})),
            _ => None,
        }
    });
    match found {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, "no such product").into_response(), // not an error worth reporting
    }
}

#[derive(Deserialize)]
struct Order {
    sku: String,
    card: String,
}

async fn create_order(State(app): State<Arc<App>>, Json(order): Json<Order>) -> Response {
    fixwire::set_tag("sku", order.sku.clone());
    tracing::info!(sku = %order.sku, "order received");

    if let Err(e) = app.reserve(&order.sku).await {
        fixwire::capture_error(&e);
        return (StatusCode::CONFLICT, "out of stock").into_response();
    }
    let id = format!(
        "ord_{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    if let Err(cause) = charge(&order.card) {
        // Handled: the customer gets an answer, Fixwire gets the error with the order.
        let mut context = Map::new();
        context.insert("id".into(), id.clone().into());
        context.insert("sku".into(), order.sku.clone().into());
        fixwire::with_scope(
            |s| s.set_context("order", context),
            || {
                fixwire::capture_error(&ChargeFailed {
                    order: id.clone(),
                    cause,
                })
            },
        );
        return (StatusCode::PAYMENT_REQUIRED, "payment declined").into_response();
    }
    (StatusCode::CREATED, Json(json!({"id": id}))).into_response()
}

impl App {
    /// Asks the inventory service to hold one item: a traced call, with trace headers.
    async fn reserve(&self, sku: &str) -> Result<(), ReserveFailed> {
        let url = format!("{}/reservations?sku={sku}", self.inventory_url);
        let call = OutgoingRequest::start("POST", &url);
        let mut request = self.http.post(&url);
        for (name, value) in call.headers() {
            request = request.header(*name, value);
        }
        match request.send().await {
            Ok(res) if res.status().is_success() => {
                call.finish(Some(res.status().as_u16()), None);
                Ok(())
            }
            Ok(res) => {
                call.finish(Some(res.status().as_u16()), None);
                Err(ReserveFailed(format!(
                    "reserving {sku}: inventory answered {}",
                    res.status().as_u16()
                )))
            }
            Err(e) => {
                call.finish(None, Some(&e));
                Err(ReserveFailed(format!("reserving {sku}: {e}")))
            }
        }
    }
}

fn charge(card: &str) -> Result<(), PaymentDeclined> {
    if card == "4000000000000002" {
        // the test card that is always declined
        return Err(PaymentDeclined {
            code: "card_declined",
        });
    }
    Ok(())
}

async fn report() -> Json<serde_json::Value> {
    let cents: Vec<u64> = Vec::new(); // today's orders: none yet
    let total: u64 = cents.iter().sum();
    // A bug: with no orders this divides by zero and panics. Fixwire reports the panic; the panic
    // layer answers 500.
    Json(json!({"average_cents": total / cents.len() as u64}))
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}

fn env(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}
