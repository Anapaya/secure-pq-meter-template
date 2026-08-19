//! Client side of the Energy Data Hackdays gateway challenge.
//!
//! Sends one message to `pq-meter-server` over SCION and prints the answer. Run it on the
//! gateway (Raspberry Pi); run `pq-meter-server` on the laptop.
//!
//! The client needs two addresses:
//!
//! * `--endhost-api`: the URL of the endhost API of its own AS. This is where the SCION
//!   stack asks for paths and for the SNAP that carries its packets. The server prints
//!   this URL when it starts.
//! * `--server`: the SCION address of the HTTP/3 server, also printed by the server.

use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use http_body_util::BodyExt;
use scion_quic::{
    h3::client::Http3Client, quic::config::QuicConfig, socket::GenericScionUdpSocket,
};
use scion_stack::stack::ScionStackBuilder;
use sciparse::address::ip_socket_addr::ScionSocketIpAddr;
use url::Url;

/// TLS name the server's certificate is issued for.
const SERVER_NAME: &str = "pq-meter-server";

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Sends meter data to pq-meter-server over SCION HTTP/3"
)]
struct Args {
    /// URL of the endhost API this client attaches to, for example
    /// `http://192.168.1.42:31000`.
    #[arg(long)]
    endhost_api: Url,

    /// SCION address of the server, for example `[2-ff00:0:212,192.168.1.42]:31337`.
    #[arg(long)]
    server: ScionSocketIpAddr,

    /// Path to POST to on the server.
    #[arg(long, default_value = "/edh/v1/hello")]
    path: String,

    /// Message to send.
    #[arg(long, default_value = "Hello from Energy Data Hackdays 2026")]
    message: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // The SDK uses rustls for its control plane; pick a crypto backend.
    scion_sdk_utils::rustls::select_ring_crypto_provider();

    // A ScionStack is the SCION equivalent of the network stack of an operating system: the
    // object sockets are opened on. It reaches the SCION network through the SNAP that the
    // endhost API points it to.
    let stack = ScionStackBuilder::new()
        .with_endhost_api(args.endhost_api)
        .with_auth_token(snap_tokens::v0::dummy_snap_token())
        .build()
        .await
        .context("building the SCION stack")?;

    let socket = stack.bind(None).await.context("opening a SCION socket")?;
    println!("client SCION address: {}", socket.local_addr());

    // HTTP/3 client from the SDK. The SCION address decides where the packets go; the URL
    // below only carries the HTTP host and path.
    let client = Http3Client::with_config(
        args.server,
        Arc::new(socket) as Arc<dyn GenericScionUdpSocket>,
        Some(SERVER_NAME.to_string()),
        // The server uses a self-signed certificate, so its identity is not verified.
        QuicConfig::builder().verify_peer(false).build(),
    );

    let body = Bytes::from(
        serde_json::to_vec(&serde_json::json!({ "message": args.message }))
            .context("encoding the message")?,
    );

    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("https://{SERVER_NAME}{}", args.path))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(())
        .context("building the request")?;

    println!("sending to {}{} ...", args.server, args.path);

    let (response, mut writer) = client
        .request(request)
        .await
        .context("sending the request")?;

    // HTTP/3 puts no ordering between the request body and the response, so the body is
    // written from its own task while the response is awaited.
    let send_body = tokio::spawn(async move {
        writer.write_chunk(body).await?;
        writer.finish().await
    });

    let response = response.await.context("waiting for the response")?;
    send_body
        .await
        .context("body task failed")?
        .context("sending the request body")?;

    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("reading the response body")?
        .to_bytes();

    println!(
        "server answered {status}: {}",
        String::from_utf8_lossy(&body)
    );

    anyhow::ensure!(status.is_success(), "server answered with {status}");
    Ok(())
}
