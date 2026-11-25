
use std::{
    collections::BTreeMap,
    sync::Arc,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request,
    Response,
    StatusCode,
    body::Incoming,
    server::conn::http1,
    service::service_fn,
    header::{CONNECTION, HOST},
};
use wasmtime::Result;
use wasmtime_wasi_http::io::TokioIo;

#[derive(Clone)]
pub struct Gateway {
    routes: Arc<BTreeMap<String, u16>>,
    client: reqwest::Client,
    internal_token: Arc<String>,
}

impl Gateway {
    pub fn new(
        routes: BTreeMap<String, u16>,
        internal_token: Arc<String>,
    ) -> Result<Self> {        let client = reqwest::Client::builder()
            .build()
            .map_err(|err| {
                wasmtime::Error::msg(
                    format!("Failed to initialize gateway HTTP client: {err}")
                )
            })?;

        Ok(Self {
            routes: Arc::new(routes),
            client,
            internal_token,
        })
    }

    async fn handle(
        &self,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        let path = req.uri().path().to_string();
        let query = req.uri().query().map(str::to_owned);

        let mut segments = path.trim_start_matches('/').splitn(2, '/');

        let service_name = segments.next().unwrap_or_default();
        let remaining_path = segments.next().unwrap_or("");

        let Some(port) = self.routes.get(service_name).copied() else {
            return response(
                StatusCode::NOT_FOUND,
                "Unknown service",
            );
        };

        let forwarded_path = format!("/{remaining_path}");

        let forwarded_uri = match query {
            Some(query) => format!(
                "http://127.0.0.1:{port}{forwarded_path}?{query}"
            ),
            None => format!(
                "http://127.0.0.1:{port}{forwarded_path}"
            ),
        };

        let method = req.method().clone();
        let headers = req.headers().clone();

        let body = match req.into_body().collect().await {
            Ok(body) => body.to_bytes(),
            Err(err) => {
                eprintln!("Gateway request body error: {err}");

                return response(
                    StatusCode::BAD_REQUEST,
                    "Failed to read request body",
                );
            }
        };

        let mut forwarded = self.client
            .request(method, forwarded_uri)
            .header(
                "x-runtime-internal-token",
                self.internal_token.as_str(),
            )
            .body(body);

        for (name, value) in &headers {
            if name == HOST || name == CONNECTION {
                continue;
            }

            forwarded = forwarded.header(name, value);
        }

        let upstream = match forwarded.send().await {
            Ok(upstream) => upstream,
            Err(err) => {
                eprintln!("Gateway upstream error: {err}");

                return response(
                    StatusCode::BAD_GATEWAY,
                    "Upstream service unavailable",
                );
            }
        };

        let status = upstream.status();
        let headers = upstream.headers().clone();

        let body = match upstream.bytes().await {
            Ok(body) => body,
            Err(err) => {
                eprintln!("Gateway response body error: {err}");

                return response(
                    StatusCode::BAD_GATEWAY,
                    "Failed to read upstream response",
                );
            }
        };

        let mut response_builder = Response::builder()
            .status(status);

        if let Some(response_headers) = response_builder.headers_mut() {
            for (name, value) in &headers {
                if name == CONNECTION {
                    continue;
                }

                response_headers.insert(name.clone(), value.clone());
            }
        }

        response_builder
            .body(Full::new(body))
            .unwrap_or_else(|_| {
                response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to construct gateway response",
                )
            })
    }
}

fn response(
    status: StatusCode,
    message: &str,
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(message.to_string())))
        .expect("valid gateway response")
}

pub async fn serve(
    gateway: Gateway,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    println!("Default gateway listener started.");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let gateway = gateway.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let gateway = gateway.clone();

                async move {
                    Ok::<_, std::convert::Infallible>(
                        gateway.handle(req).await
                    )
                }
            });

            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service,
                )
                .await
            {
                eprintln!(
                    "Gateway connection {peer_addr} failed: {err}"
                );
            }
        });
    }
}

