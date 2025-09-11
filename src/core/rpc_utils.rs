use std::error::Error;

use axum::response::Response;
use http::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    pub id: serde_json::Value,
}

#[derive(Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub result: serde_json::Value,
    pub id: serde_json::Value,
}

#[derive(Serialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub error: JsonRpcError,
    pub id: serde_json::Value,
}

#[derive(Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

pub fn is_batch_request(request: &[u8]) -> bool {
    request.starts_with(b"[")
}

pub fn parse_batch(request: &[u8]) -> Result<Vec<JsonRpcRequest>, Box<dyn Error>> {
    let Some(request) = serde_json::from_slice::<Vec<JsonRpcRequest>>(request).ok() else {
        return Err("Failed to parse JSON-RPC request".into());
    };
    for req in &request {
        if req.jsonrpc != "2.0" {
            return Err("Invalid JSON-RPC version".into());
        }
    }
    Ok(request)
}

pub fn parse_request(request: &[u8]) -> Result<JsonRpcRequest, Box<dyn Error>> {
    let Some(request) = serde_json::from_slice::<JsonRpcRequest>(request).ok() else {
        return Err("Failed to parse JSON-RPC request".into());
    };
    if request.jsonrpc != "2.0" {
        return Err("Invalid JSON-RPC version".into());
    }
    Ok(request)
}

pub fn wrap_json_rpc(result: serde_json::Value, id: serde_json::Value) -> serde_json::Value {
    let response = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        result,
        id,
    };
    serde_json::to_value(response).expect("Failed to serialize response")
}

pub fn wrap_json_rpc_error(code: i32, message: String, id: serde_json::Value) -> serde_json::Value {
    let error_response = JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        error: JsonRpcError { code, message },
        id,
    };
    serde_json::to_value(error_response).expect("Failed to serialize error response")
}

pub fn make_response(response: serde_json::Value) -> Response {
    let response_body = serde_json::to_vec(&response).unwrap();
    tracing::debug!("Response body: {}", String::from_utf8_lossy(&response_body));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(response_body.into())
        .expect("Failed to create response")
}

pub fn wrap_error(code: i32, message: String, id: serde_json::Value) -> Response {
    let error_response = JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        error: JsonRpcError { code, message },
        id,
    };
    let response_body = serde_json::to_vec(&error_response).unwrap();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(response_body.into())
        .expect("Failed to create error response")
}

pub fn remove_duplicate_input_and_data(params: &mut serde_json::Value) -> serde_json::Value {
    if let Some(obj) = params.as_object_mut() {
        if obj.contains_key("input") && obj.contains_key("data") {
            obj.remove("input");
        }
    }
    params.clone()
}
