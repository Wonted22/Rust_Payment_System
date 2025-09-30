use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{post},
    Router,
};
use tokio::net::TcpListener;
use serde::{Deserialize, Serialize};
use chrono::{Utc, NaiveDateTime};
use uuid::Uuid;
use dotenvy::dotenv;
use std::{env, time::Duration};
use sqlx::{PgPool, postgres::PgPoolOptions};
use anyhow::Result;
use reqwest::Client;


// --- 0. STATE MANAGEMENT ---

// State struct holding DB pool, API key, and Reqwest client
#[derive(Clone)]
struct AppState {
    db: PgPool,
    api_key: String,
    http_client: Client,
}

// --- 1. MODELS ---

// Payment Request (Inbound Data)
#[derive(Debug, Deserialize)]
pub struct PaymentRequest {
    pub amount: i32, // Cents/Minor Unit
    pub currency: String, // E.g., "USD", "TRY"
    pub card_number: String,
    pub expiry_month: i32,
    pub expiry_year: i32,
    pub cvv: String,
}

// Payment Response (Outbound Data)
#[derive(Debug, Serialize)]
pub struct PaymentResponse {
    pub success: bool,
    pub transaction_id: String,
    pub message: String,
    pub timestamp: NaiveDateTime,
}

impl PaymentResponse {
    pub fn new_success(transaction_id: String, message: String) -> Self {
        PaymentResponse {
            success: true,
            transaction_id,
            message,
            timestamp: Utc::now().naive_utc(),
        }
    }

    pub fn new_failure(transaction_id: String, message: String) -> Self {
        PaymentResponse {
            success: false,
            transaction_id,
            message,
            timestamp: Utc::now().naive_utc(),
        }
    }
}

// TRANSACTION MODELS
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Transaction {
    pub id: i32,
    pub transaction_uuid: Uuid,
    pub amount: i32,
    pub currency: String,
    pub status: String,
    pub masked_card_number: String,
    pub created_at: NaiveDateTime,
}

// --- 2. ERROR HANDLING (ADVANCED) ---

// Advanced error handling: AppError
#[derive(Debug)]
enum AppError {
    InternalServerError(String),
    BadRequest(String),
    DatabaseError(sqlx::Error),
    EnvironmentError(String),
    GatewayError(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for AppError {}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::DatabaseError(err)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        AppError::GatewayError(format!("External gateway call failed: {}", err))
    }
}

// Handle error conversion
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::InternalServerError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::DatabaseError(err) => {
                eprintln!("SQLx Error: {:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR, 
                    "Database operation failed.".to_string()
                )
            },
            AppError::EnvironmentError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::GatewayError(msg) => (StatusCode::BAD_GATEWAY, msg),
        };

        (status, Json(serde_json::json!({"error": error_message}))).into_response()
    }
}


// --- 3. EXTERNAL GATEWAY SIMULATION ---

async fn call_external_payment_gateway(
    _client: &Client, 
    api_key: &str, 
    data: &PaymentRequest, 
    _transaction_uuid: &Uuid
) -> Result<(String, String), AppError> { 
    
    if api_key.is_empty() {
        return Err(AppError::EnvironmentError("API Key is missing.".to_string()));
    }
    
    // Simulation Rule: Card starting with 4000 fails
    if data.card_number.starts_with("4000") {
        return Ok(("FAILED".to_string(), "Card declined: Insufficient funds (Simulation).".to_string()));
    }
    
    println!("-> External Gateway Call Successful. Key Used: {}...", &api_key[..5]);

    Ok(("SUCCESS".to_string(), "Payment successfully processed by external gateway.".to_string())) 
}


// --- 4. HANDLER FUNCTION ---

async fn process_payment(
    State(state): State<AppState>,
    Json(payment_data): Json<PaymentRequest>,
) -> Result<Json<PaymentResponse>, AppError> {
    
    // 1. Basic Validation
    if payment_data.amount <= 0 {
        return Err(AppError::BadRequest("Payment amount must be greater than zero.".to_string()));
    }
    if payment_data.card_number.len() < 12 || payment_data.card_number.len() > 19 {
        return Err(AppError::BadRequest("Invalid card number.".to_string()));
    }
    
    let masked_card = format!("XXXX-XXXX-XXXX-{}", &payment_data.card_number[payment_data.card_number.len() - 4..]);
    let transaction_uuid = Uuid::new_v4();

    // 2. EXTERNAL GATEWAY CALL
    let (status, response_message) = call_external_payment_gateway(
        &state.http_client, 
        &state.api_key, 
        &payment_data, 
        &transaction_uuid
    ).await?;

    
    // 3. PERSIST TRANSACTION TO DATABASE
    sqlx::query!(
        r#"
        INSERT INTO transactions (transaction_uuid, amount, currency, status, masked_card_number)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        transaction_uuid,
        payment_data.amount,
        payment_data.currency,
        status,
        masked_card
    )
    .execute(&state.db)
    .await?; 


    // 4. Send Response to Customer
    
    if status == "SUCCESS" {
        println!("Successful payment: {} ({} {})", 
            masked_card, payment_data.amount, payment_data.currency);
        
        Ok(Json(PaymentResponse::new_success(
            transaction_uuid.to_string(),
            response_message
        )))
    } else {
        eprintln!("Failed payment: {} ({} {})", 
            masked_card, payment_data.amount, payment_data.currency);

        Ok(Json(PaymentResponse::new_failure(
            transaction_uuid.to_string(),
            response_message
        )))
    }
}

// --- 5. MAIN FUNCTION AND ROUTE SETUP ---

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    
    // Database connection
    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set in the .env file");

    let db_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("Failed to create PostgreSQL connection pool.");

    println!("-> Successfully connected to the database.");
    
    let api_key = env::var("PAYMENT_GATEWAY_API_KEY")
        .expect("PAYMENT_GATEWAY_API_KEY must be set in the .env file");

    // Initialize reqwest client
    let http_client = Client::builder()
        .timeout(Duration::from_secs(10)) 
        .build()
        .expect("Failed to create HTTP client.");
        
    let app_state = AppState { db: db_pool, api_key, http_client };

    // Application routes
    // Rate limiting katmanı kaldırıldı.
    let app = Router::new()
        .route("/api/payment", post(process_payment))
        // .layer(rate_limit_layer) <--- KALDIRILDI
        .with_state(app_state);

    // Bind and serve
    let listener = TcpListener::bind("127.0.0.1:3000")
        .await
        .map_err(|e| AppError::InternalServerError(format!("Failed to start TCP listener: {}", e)))?;

    let addr = listener.local_addr().unwrap();
    println!("-> Ultra Secure Payment API is running: http://{}", addr);

    axum::serve(listener, app)
        .await
        .map_err(|e| AppError::InternalServerError(format!("Server error: {}", e)))?;
    
    Ok(())
}