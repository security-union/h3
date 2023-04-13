use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{future, StreamExt};
use futures::{future::poll_fn, AsyncRead};
use http::{Method, Request, StatusCode};
use rustls::{Certificate, PrivateKey};
use structopt::StructOpt;
use tokio::{fs::File, io::AsyncReadExt, time::sleep};
use tracing::{error, info, trace_span};

use h3::{
    error::ErrorLevel,
    quic::{self, BidiStream, RecvStream, SendStream},
    server::{Config, Connection, RequestStream},
    webtransport::server::WebTransportSession,
    Protocol,
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

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
    server_config.transport = Arc::new(transport_config);
    let endpoint = quinn::Endpoint::server(server_config, opt.listen)?;

    info!("listening on {}", opt.listen);

    // 1. Configure h3 server, since this is a webtransport server, we need to enable webtransport, connect, and datagram
    let mut h3_config = Config::new();
    h3_config.enable_webtransport(true);
    h3_config.enable_connect(true);
    h3_config.enable_datagram(true);
    h3_config.max_webtransport_sessions(16);
    h3_config.send_grease(true);

    // 2. Accept new quic connections and spawn a new task to handle them
    while let Some(new_conn) = endpoint.accept().await {
        trace_span!("New connection being attempted");

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

                    // tracing::info!("Establishing WebTransport session");
                    // // 3. TODO: Conditionally, if the client indicated that this is a webtransport session, we should accept it here, else use regular h3.
                    // // if this is a webtransport session, then h3 needs to stop handing the datagrams, bidirectional streams, and unidirectional streams and give them
                    // // to the webtransport session.

                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(h3_conn).await {
                            tracing::error!("Failed to handle connection: {err:?}");
                        }
                    });
                    // let mut session: WebTransportSession<_, Bytes> =
                    //     WebTransportSession::accept(h3_conn).await.unwrap();
                    // tracing::info!("Finished establishing webtransport session");
                    // // 4. Get datagrams, bidirectional streams, and unidirectional streams and wait for client requests here.
                    // // h3_conn needs to handover the datagrams, bidirectional streams, and unidirectional streams to the webtransport session.
                    // let result = handle.await;
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

async fn handle_connection<C>(mut conn: Connection<C>) -> Result<()>
where
    C: 'static + Send + quic::Connection,
    <C::SendStream as h3::quic::SendStream>::Error: 'static + std::error::Error + Send + Sync,
{
    // 3. TODO: Conditionally, if the client indicated that this is a webtransport session, we should accept it here, else use regular h3.
    // if this is a webtransport session, then h3 needs to stop handing the datagrams, bidirectional streams, and unidirectional streams and give them
    // to the webtransport session.

    loop {
        match conn.accept().await {
            Ok(Some((req, stream))) => {
                info!("new request: {:#?}", req);

                let ext = req.extensions();
                match req.method() {
                    &Method::CONNECT if ext.get::<Protocol>() == Some(&Protocol::WEB_TRANSPORT) => {
                        tracing::info!("Peer wants to initiate a webtransport session");

                        tracing::info!("Handing over connection to WebTransport");
                        let session = WebTransportSession::accept(req, stream, conn).await?;
                        tracing::info!("Established webtransport session");
                        // 4. Get datagrams, bidirectional streams, and unidirectional streams and wait for client requests here.
                        // h3_conn needs to handover the datagrams, bidirectional streams, and unidirectional streams to the webtransport session.
                        handle_session_and_echo_all_inbound_messages(session).await?;

                        return Ok(());
                    }
                    _ => {
                        tracing::info!(?req, "Received request");
                    }
                }
            }

            // indicating no more streams to be received
            Ok(None) => {
                break;
            }

            Err(err) => {
                error!("Error on accept {}", err);
                match err.get_error_level() {
                    ErrorLevel::ConnectionError => break,
                    ErrorLevel::StreamError => continue,
                }
            }
        }
    }
    Ok(())
}

/// This method will echo all inbound datagrams, unidirectional and bidirectional streams.
#[tracing::instrument(level = "info", skip(session))]
async fn handle_session_and_echo_all_inbound_messages<C>(
    session: WebTransportSession<C>,
) -> anyhow::Result<()>
where
    C: 'static + Send + h3::quic::Connection,
    <C::SendStream as h3::quic::SendStream>::Error: 'static + std::error::Error + Send + Sync,
{
    loop {
        tokio::select! {
            datagram = session.read_datagram() => {
                let datagram = datagram?;
                if let Some((_, datagram)) = datagram {
                    tracing::info!("Responding with {datagram:?}");
                    // Put something before to make sure encoding and decoding works and don't just
                    // pass through
                    let mut resp = BytesMut::from(&b"Response: "[..]);
                    resp.put(datagram);

                    session.send_datagram(resp).unwrap();
                    tracing::info!("Finished sending datagram");
                }
            }
            stream = session.accept_uni() => {
                let (id, mut stream) = stream?.unwrap();
                tracing::info!("Received uni stream {:?} for {id:?}", stream.recv_id());
                if let Some(bytes) = poll_fn(|cx| stream.poll_data(cx)).await? {
                    tracing::info!("Received data {:?}", bytes);
                }
            }
            stream = session.accept_bi() => {
                if let Some(h3::webtransport::server::AcceptedBi::BidiStream(_, mut send, mut recv)) = stream? {

                tracing::info!("Got bi stream");
                let mut message = BytesMut::new();
                while let Some(bytes) = poll_fn(|cx| recv.poll_data(cx)).await? {
                    tracing::info!("Received data {:?}", bytes);
                    message.put(bytes);

                }

                tracing::info!("Got message: {message:?}");

                let mut response = BytesMut::new();
                response.put(format!("I got your message of {} bytes: ", message.len()).as_bytes());
                response.put(message);

                send.write_all(response).await.context("Failed to respond")?;
                }
            }
            else => {
                break
            }
        }
    }

    tracing::info!("Finished handling session");

    Ok(())
}