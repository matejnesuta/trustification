use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use actix_web::middleware::Logger;
use actix_web::web::{self, Bytes};
use actix_web::{App, HttpResponse, HttpServer, Responder};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use trustification_storage::{Object, Storage};

struct AppState {
    storage: RwLock<Storage>,
}

type SharedState = Arc<AppState>;

pub async fn run<B: Into<SocketAddr>>(storage: Storage, bind: B) -> Result<(), anyhow::Error> {
    let storage = RwLock::new(storage);
    let state = Arc::new(AppState { storage });
    let addr = bind.into();
    tracing::debug!("listening on {}", addr);
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(web::PayloadConfig::new(10 * 1024 * 1024))
            .app_data(web::Data::new(state.clone()))
            .service(web::resource("/healthz").to(health))
            .service(
                web::scope("/api/v1")
                    .route("/vex", web::get().to(query_vex))
                    .route("/vex", web::post().to(publish_vex)),
            )
    })
    .bind(&addr)?
    .run()
    .await?;
    Ok(())
}

async fn fetch_object(storage: &Storage, key: &str) -> HttpResponse {
    match storage.get(&key).await {
        Ok(obj) => {
            tracing::trace!("Retrieved object compressed: {}", obj.compressed);
            if obj.compressed {
                let mut out = Vec::new();
                match ::zstd::stream::copy_decode(&obj.data[..], &mut out) {
                    Ok(_) => HttpResponse::Ok().body(out),
                    Err(_) => HttpResponse::InternalServerError().body("Unable to decode object"),
                }
            } else {
                HttpResponse::Ok().body(obj.data)
            }
        }
        Err(e) => {
            tracing::warn!("Unable to locate object with key {}: {:?}", key, e);
            HttpResponse::NotFound().finish()
        }
    }
}

async fn health() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[derive(Debug, Deserialize)]
struct QueryParams {
    cve: Option<String>,
    advisory: Option<String>,
}

async fn query_vex(state: web::Data<SharedState>, params: web::Query<QueryParams>) -> HttpResponse {
    let params = params.into_inner();
    let advisory = if let Some(advisory) = params.advisory {
        tracing::trace!("Querying VEX using advisory {}", advisory);
        advisory
    } else if let Some(cve) = params.cve {
        return HttpResponse::BadRequest()
            .body("CVE lookup is not yet supported")
            .into();
    } else {
        return HttpResponse::BadRequest().body("Missing valid advisory or CVE").into();
    };

    let storage = state.storage.read().await;
    fetch_object(&storage, &advisory).await
}

#[derive(Debug, Deserialize)]
struct PublishParams {
    advisory: Option<String>,
}

async fn publish_vex(state: web::Data<SharedState>, params: web::Query<PublishParams>, data: Bytes) -> HttpResponse {
    let params = params.into_inner();
    let advisory = if let Some(advisory) = params.advisory {
        advisory.to_string()
    } else {
        match serde_json::from_slice::<csaf::Csaf>(&data) {
            Ok(data) => data.document.tracking.id,
            Err(e) => {
                tracing::warn!("Unknown input format: {:?}", e);
                return HttpResponse::BadRequest().into();
            }
        }
    };

    let storage = state.storage.write().await;
    let mut out = Vec::new();
    let (data, compressed) = match zstd::stream::copy_encode(&data[..], &mut out, 3) {
        Ok(_) => (&out[..], true),
        Err(_) => (&data[..], false),
    };
    tracing::debug!("Storing new VEX with id: {}, compressed: {}", advisory, compressed);
    let value = Object::new(&advisory, std::collections::HashMap::new(), data, compressed);
    match storage.put(&advisory, value).await {
        Ok(_) => {
            let msg = format!("VEX of size {} stored successfully", &data[..].len());
            tracing::trace!(msg);
            HttpResponse::Created().body(msg)
        }
        Err(e) => {
            let msg = format!("Error storing VEX: {:?}", e);
            tracing::warn!(msg);
            HttpResponse::InternalServerError().body(msg)
        }
    }
}