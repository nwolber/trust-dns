// Copyright 2015-2016 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::io;
use std::net::SocketAddr;

use futures::sync::mpsc::unbounded;
use futures::{future, Future, IntoFuture};
use openssl::pkcs12::ParsedPkcs12;
use openssl::pkey::{PKeyRef, Private};
use openssl::ssl::{SslConnector, SslContextBuilder, SslMethod, SslOptions};
use openssl::stack::Stack;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::{X509, X509Ref};
use tokio_openssl::{SslConnectorExt, SslStream as TokioTlsStream};
use tokio_tcp::TcpStream as TokioTcpStream;

use trust_dns_proto::tcp::TcpStream;
use trust_dns_proto::xfer::BufStreamHandle;

pub trait TlsIdentityExt {
    fn identity(&mut self, pkcs12: &ParsedPkcs12) -> io::Result<()> {
        self.identity_parts(&pkcs12.cert, &pkcs12.pkey, pkcs12.chain.as_ref())
    }

    fn identity_parts(
        &mut self,
        cert: &X509Ref,
        pkey: &PKeyRef<Private>,
        chain: Option<&Stack<X509>>,
    ) -> io::Result<()>;
}

impl TlsIdentityExt for SslContextBuilder {
    fn identity_parts(
        &mut self,
        cert: &X509Ref,
        pkey: &PKeyRef<Private>,
        chain: Option<&Stack<X509>>,
    ) -> io::Result<()> {
        self.set_certificate(cert)?;
        self.set_private_key(pkey)?;
        self.check_private_key()?;
        if let Some(chain) = chain {
            for cert in chain {
                self.add_extra_chain_cert(cert.to_owned())?;
            }
        }
        Ok(())
    }
}

/// A TlsStream counterpart to the TcpStream which embeds a secure TlsStream
pub type TlsStream = TcpStream<TokioTlsStream<TokioTcpStream>>;

fn new(certs: Vec<X509>, pkcs12: Option<ParsedPkcs12>) -> io::Result<SslConnector> {
    let mut tls = SslConnector::builder(SslMethod::tls()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("tls error: {}", e),
        )
    })?;

    // mutable reference block
    {
        let openssl_ctx_builder = &mut tls;

        // only want to support current TLS versions, 1.2 or future
        openssl_ctx_builder.set_options(
            SslOptions::NO_SSLV2
                | SslOptions::NO_SSLV3
                | SslOptions::NO_TLSV1
                | SslOptions::NO_TLSV1_1,
        );

        let mut store = X509StoreBuilder::new().map_err(|e| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("tls error: {}", e),
            )
        })?;

        for cert in certs {
            store.add_cert(cert).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("tls error: {}", e),
                )
            })?;
        }

        openssl_ctx_builder
            .set_verify_cert_store(store.build())
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("tls error: {}", e),
                )
            })?;

        // if there was a pkcs12 associated, we'll add it to the identity
        if let Some(pkcs12) = pkcs12 {
            openssl_ctx_builder.identity(&pkcs12)?;
        }
    }
    Ok(tls.build())
}

/// Initializes a TlsStream with an existing tokio_tls::TlsStream.
///
/// This is intended for use with a TlsListener and Incoming connections
pub fn tls_stream_from_existing_tls_stream(
    stream: TokioTlsStream<TokioTcpStream>,
    peer_addr: SocketAddr,
) -> (TlsStream, BufStreamHandle) {
    let (message_sender, outbound_messages) = unbounded();
    let message_sender = BufStreamHandle::new(message_sender);

    let stream = TcpStream::from_stream_with_receiver(stream, peer_addr, outbound_messages);

    (stream, message_sender)
}

/// A builder for the TlsStream
pub struct TlsStreamBuilder {
    ca_chain: Vec<X509>,
    identity: Option<ParsedPkcs12>,
}

impl TlsStreamBuilder {
    /// A builder for associating trust information to the `TlsStream`.
    pub fn new() -> Self {
        TlsStreamBuilder {
            ca_chain: vec![],
            identity: None,
        }
    }

    /// Add a custom trusted peer certificate or certificate auhtority.
    ///
    /// If this is the 'client' then the 'server' must have it associated as it's `identity`, or have had the `identity` signed by this
    pub fn add_ca(&mut self, ca: X509) {
        self.ca_chain.push(ca);
    }

    /// Client side identity for client auth in TLS (aka mutual TLS auth)
    #[cfg(feature = "mtls")]
    pub fn identity(&mut self, pkcs12: ParsedPkcs12) {
        self.identity = Some(pkcs12);
    }

    /// Creates a new TlsStream to the specified name_server
    ///
    /// [RFC 7858](https://tools.ietf.org/html/rfc7858), DNS over TLS, May 2016
    ///
    /// ```text
    /// 3.2.  TLS Handshake and Authentication
    ///
    ///   Once the DNS client succeeds in connecting via TCP on the well-known
    ///   port for DNS over TLS, it proceeds with the TLS handshake [RFC5246],
    ///   following the best practices specified in [BCP195].
    ///
    ///   The client will then authenticate the server, if required.  This
    ///   document does not propose new ideas for authentication.  Depending on
    ///   the privacy profile in use (Section 4), the DNS client may choose not
    ///   to require authentication of the server, or it may make use of a
    ///   trusted Subject Public Key Info (SPKI) Fingerprint pin set.
    ///
    ///   After TLS negotiation completes, the connection will be encrypted and
    ///   is now protected from eavesdropping.
    /// ```
    ///
    /// # Arguments
    ///
    /// * `name_server` - IP and Port for the remote DNS resolver
    /// * `dns_name` - The DNS name, Subject Public Key Info (SPKI) name, as associated to a certificate
    pub fn build(
        self,
        name_server: SocketAddr,
        dns_name: String,
    ) -> (
        Box<Future<Item = TlsStream, Error = io::Error> + Send>,
        BufStreamHandle,
    ) {
        let (message_sender, outbound_messages) = unbounded();
        let message_sender = BufStreamHandle::new(message_sender);

        let tls_connector = match new(self.ca_chain, self.identity) {
            Ok(c) => c,
            Err(e) => {
                return (
                    Box::new(future::err(e).into_future().map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            format!("tls error: {}", e),
                        )
                    })),
                    message_sender,
                )
            }
        };

        let tcp = TokioTcpStream::connect(&name_server);

        // This set of futures collapses the next tcp socket into a stream which can be used for
        //  sending and receiving tcp packets.
        let stream = Box::new(
            tcp.and_then(move |tcp_stream| {
                tls_connector
                    .connect_async(&dns_name, tcp_stream)
                    .map(move |s| {
                        TcpStream::from_stream_with_receiver(s, name_server, outbound_messages)
                    })
                    .map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            format!("tls error: {}", e),
                        )
                    })
            }).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("tls error: {}", e),
                )
            }),
        );

        (stream, message_sender)
    }
}
