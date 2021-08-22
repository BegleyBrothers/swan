use serde::Serialize;
use std::convert::Infallible;
use thiserror::Error;
// use warp::{http::StatusCode, Rejection, Reply};

#[derive(Error, Debug)]
pub enum Error {
    #[error("error reading file: {0}")]
    ReadFileError(#[from] std::io::Error),
    #[error("http client error: {0}")]
    HyperHttpError(#[from] hyper::http::Error),
    #[error("http client error: {0}")]
    HypeError(#[from] hyper::Error),
    #[error("http client error: {0}")]
    JSONError(#[from] serde_json::error::Error),
}

#[derive(Serialize)]
struct ErrorResponse {
    message: String,
}

// impl warp::reject::Reject for Error {}

// pub async fn handle_rejection(err: Rejection) -> std::result::Result<impl Reply, Infallible> {
//     let code;
//     let message;

//     if err.is_not_found() {
//         code = StatusCode::NOT_FOUND;
//         message = "Not Found";
//     } else if let Some(_) = err.find::<warp::filters::body::BodyDeserializeError>() {
//         code = StatusCode::BAD_REQUEST;
//         message = "Invalid Body";
//     } else if let Some(e) = err.find::<Error>() {
//         match e {
//             _ => {
//                 eprintln!("unhandled application error: {:?}", err);
//                 code = StatusCode::INTERNAL_SERVER_ERROR;
//                 message = "Internal Server Error";
//             }
//         }
//     } else if let Some(_) = err.find::<warp::reject::MethodNotAllowed>() {
//         code = StatusCode::METHOD_NOT_ALLOWED;
//         message = "Method Not Allowed";
//     } else {
//         eprintln!("unhandled error: {:?}", err);
//         code = StatusCode::INTERNAL_SERVER_ERROR;
//         message = "Internal Server Error";
//     }

//     let json = warp::reply::json(&ErrorResponse {
//         message: message.into(),
//     });

//     Ok(warp::reply::with_status(json, code))
// }