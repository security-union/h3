use std::{net::SocketAddr, path::{PathBuf, Path}, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use http::{Request, StatusCode};
use rustls::{Certificate, PrivateKey};
use structopt::StructOpt;
use tokio::{fs::File, io::AsyncReadExt, time::sleep};
use tracing::{error, info, trace_span};
use anyhow::{Result, anyhow};

use h3::{
    error::ErrorLevel,
    quic::BidiStream,
    server::{Config, RequestStream, WebTransportSession},
};
use h3_quinn::quinn;
use tracing_subscriber::prelude::*;

#[derive(StructOpt, Debug)]
#[structopt(name = "server")]
struct Opt {
    #[structopt(
        name = "dir",
        short,
        long,
        help = "Root directory of the files to serve. \
                If omitted, server will respond OK."
    )]
    pub root: Option<PathBuf>,

    #[structopt(
        short,
        long,
        default_value = "127.0.0.1:4433",
        help = "What address:port to listen for new connections"
    )]
    pub listen: SocketAddr,

    #[structopt(flatten)]
    pub certs: Certs,
}

#[derive(StructOpt, Debug)]
pub struct Certs {
    #[structopt(
        long,
        short,
        default_value = "examples/localhost.crt",
        help = "Certificate for TLS. If present, `--key` is mandatory."
    )]
    pub cert: PathBuf,

    #[structopt(
        long,
        short,
        default_value = "examples/localhost.key",
        help = "Private key for the certificate."
    )]
    pub key: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 0. Setup tracing
    #[cfg(not(feature = "tracing-tree"))]
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    #[cfg(feature = "tracing-tree")]
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_tree::HierarchicalLayer::new(4).with_bracketed_fields(true))
        .init();

    // process cli arguments

    let opt = Opt::from_args();

    tracing::info!("Opt: {opt:#?}");
    let root = if let Some(root) = opt.root {
        if !root.is_dir() {
            return Err(format!("{}: is not a readable directory", root.display()).into());
        } else {
            info!("serving {}", root.display());
            Arc::new(Some(root))
        }
    } else {
        Arc::new(None)
    };

    let Certs { cert, key } = opt.certs;

    // create quinn server endpoint and bind UDP socket

    // both cert and key must be DER-encoded
    let cert = Certificate(std::fs::read(cert)?);
    let key = PrivateKey(std::fs::read(key)?);

    let mut tls_config = rustls::ServerConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;

    tls_config.max_early_data_size = u32::MAX;
    let alpn: Vec<Vec<u8>> = vec![
        b"h3".to_vec(),
        b"h3-32".to_vec(),
        b"h3-31".to_vec(),
        b"h3-30".to_vec(),
        b"h3-29".to_vec(),
    ];
    tls_config.alpn_protocols = alpn;

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let (endpoint, mut incoming) = quinn::Endpoint::server(server_config, opt.listen)?;

    info!("listening on {}", opt.listen);

    // 1. Configure h3 server, since this is a webtransport server, we need to enable webtransport, connect, and datagram
    let mut h3_config = Config::new();
    h3_config.enable_webtransport(true);
    h3_config.enable_connect(true);
    h3_config.enable_datagram(true);
    h3_config.max_webtransport_sessions(16);
    h3_config.send_grease(true);

    // 2. Accept new quic connections and spawn a new task to handle them
    while let Some(new_conn) = incoming.next().await {
        trace_span!("New connection being attempted");

        let root = root.clone();

        tokio::spawn(async move {
            match new_conn.await {
                Ok(conn) => {
                    info!("new http3 established");
                    let h3_conn = h3::server::Connection::with_config(
                        h3_quinn::Connection::new(conn),
                        h3_config,
                    )
                    .await
                    .unwrap();

                    tracing::info!("Establishing WebTransport session");
                    // 3. TODO: Conditionally, if the client indicated that this is a webtransport session, we should accept it here, else use regular h3.
                    // if this is a webtransport session, then h3 needs to stop handing the datagrams, bidirectional streams, and unidirectional streams and give them 
                    // to the webtransport session.
                    let mut session: WebTransportSession<_, Bytes> =
                        WebTransportSession::new(h3_conn).await.unwrap();
                    tracing::info!("Finished establishing webtransport session");
                    // 4. Get datagrams, bidirectional streams, and unidirectional streams and wait for client requests here.
                    // h3_conn needs to handover the datagrams, bidirectional streams, and unidirectional streams to the webtransport session.
                    // session.echo_all_web_transport_requests().await;
                    let handle = session.echo_all_web_transport_requests().await;
                    let result = handle.await;
                }
                Err(err) => {
                    error!("accepting connection failed: {:?}", err);
                }
            }
        });
    }

    // shut down gracefully
    // wait for connections to be closed before exiting
    endpoint.wait_idle().await;

    Ok(())
}

async fn handle_request(
    root: Arc<Path>,
    (mut send, recv): (quinn::SendStream, quinn::RecvStream),
) -> Result<()> {
    let req = recv
        .read_to_end(64 * 1024)
        .await
        .map_err(|e| anyhow!("failed reading request: {}", e))?;

    // Write the response
    send.write_all(&req)
        .await
        .map_err(|e| anyhow!("failed to send response: {}", e))?;
    // Gracefully terminate the stream
    send.finish()
        .await
        .map_err(|e| anyhow!("failed to shutdown stream: {}", e))?;
    info!("complete");
    Ok(())
}
