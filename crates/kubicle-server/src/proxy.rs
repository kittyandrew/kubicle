//! The image proxy for remote images the user allowed: the sender sees kubicle's address, never the reader's.
//! It resolves the host once, refuses any address that is not public, connects to exactly that address and
//! checks every redirect hop the same way. Nothing from the browser's request reaches the remote server.

use std::{net::IpAddr, net::Ipv4Addr, net::SocketAddr, time::Duration};

use axum::{body::Body, http::Request, http::Response, http::StatusCode, http::header};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use kubicle_mail::{render::mime_type, render::served};
use kubicle_mailbox::Error;
use rustls::pki_types::ServerName;
use tokio::{io::AsyncRead, io::AsyncWrite, net::TcpStream};
use url::{Position, Url};

use crate::config::client_tls;

const MAX_BYTES: usize = 10 << 20;
const TIMEOUT: Duration = Duration::from_secs(10); // for the whole fetch, redirects included
const MAX_REDIRECTS: usize = 3;
/// Fetches at once, each buffering up to `MAX_BYTES`: a page of slow images held 10 MiB each for 10 s, without end.
const MAX_FETCHES: usize = 8;
/// Web ports only, so the proxy cannot be pointed at a mail or database port of a public host.
const PORTS: &[u16] = &[80, 443, 8080, 8443];

fn bad_gateway(why: impl std::fmt::Display) -> Error { Error::status(StatusCode::BAD_GATEWAY, format!("image proxy: {why}")) }

fn bad_url() -> Error { Error::status(StatusCode::BAD_REQUEST, "image proxy: not an http(s) URL on a web port") }

pub struct Proxy(tokio_rustls::TlsConnector, tokio::sync::Semaphore);

impl Proxy {
    /// Trusts the public web PKI only: the proxy fetches from the internet, never from front.
    #[must_use]
    pub fn new() -> Self {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        Self(tokio_rustls::TlsConnector::from(client_tls(roots)), tokio::sync::Semaphore::new(MAX_FETCHES))
    }

    /// Fetches one image the caller has verified a signed URL for, and answers with it.
    pub async fn fetch(&self, url: &str) -> Result<Response<Body>, Error> {
        let _turn = self.1.acquire().await.expect("the semaphore is never closed");
        tokio::time::timeout(TIMEOUT, self.fetch_inner(url)).await.map_err(|_| bad_gateway("timed out"))?
    }

    async fn fetch_inner(&self, url: &str) -> Result<Response<Body>, Error> {
        let mut url = Url::parse(url).map_err(|_| bad_url())?;
        for _ in 0..=MAX_REDIRECTS {
            let response = self.get(&url).await?;
            let status = response.status();
            if status.is_redirection() {
                let location = response.headers().get(header::LOCATION).and_then(|l| l.to_str().ok()).ok_or_else(|| bad_gateway(status))?;
                url = url.join(location).map_err(|_| bad_url())?;
                continue;
            }
            return if status == StatusCode::OK { image_response(response).await } else { Err(bad_gateway(status)) };
        }
        Err(bad_gateway("too many redirects"))
    }

    async fn get(&self, url: &Url) -> Result<Response<Incoming>, Error> {
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            _ => return Err(bad_url()),
        };
        let port = url.port_or_known_default().filter(|p| PORTS.contains(p)).ok_or_else(bad_url)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(bad_url());
        }
        let host = url.host_str().ok_or_else(bad_url)?.trim_matches(['[', ']']); // an IPv6 literal without its brackets
        // One lookup, none for an IP literal; every answer must be public, so a name with one private record is
        // refused whole.
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.map_err(bad_gateway)?.collect();
        if addrs.is_empty() || !addrs.iter().all(|a| is_public(a.ip())) {
            return Err(Error::status(StatusCode::FORBIDDEN, "image proxy: the host resolves to a non-public address"));
        }
        let server_name = ServerName::try_from(host.to_owned()).map_err(|_| bad_url())?; // a name, or the IP literal
        let tcp = TcpStream::connect(addrs[0]).await.map_err(bad_gateway)?;
        let request = Request::get(&url[Position::BeforePath..Position::AfterQuery])
            .header(header::HOST, &url[Position::BeforeHost..Position::AfterPort])
            .header(header::USER_AGENT, "Mozilla/5.0 (compatible; kubicle image proxy)")
            .header(header::ACCEPT, "image/avif,image/webp,image/png,image/*;q=0.8")
            .header(header::ACCEPT_ENCODING, "identity")
            .body(Empty::<Bytes>::new())
            .map_err(|_| bad_url())?;
        if https {
            let tls = self.0.connect(server_name, tcp).await.map_err(bad_gateway)?;
            send(tls, request).await
        } else {
            send(tcp, request).await
        }
    }
}

async fn send<S>(io: S, request: Request<Empty<Bytes>>) -> Result<Response<Incoming>, Error>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(bad_gateway)?;
    tokio::spawn(connection); // drives the socket; ends when the response body is read or dropped
    sender.send_request(request).await.map_err(bad_gateway)
}

/// The remote image as kubicle's own response: a fresh header set, so no cookie or other remote header passes.
async fn image_response(response: Response<Incoming>) -> Result<Response<Body>, Error> {
    let headers = response.headers();
    let mime = mime_type(headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default());
    // `mime_type` already reads image/jpg as image/jpeg.
    let content_type = kubicle_api::image_type(&mime).ok_or_else(|| bad_gateway("not a raster image"))?;
    if headers.get(header::CONTENT_ENCODING).is_some_and(|e| e.as_bytes() != b"identity") {
        return Err(bad_gateway("compressed")); // asked for identity; a compressed body is not a plain image
    }
    let declared_len = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok());
    if declared_len.is_some_and(|len| len > MAX_BYTES) {
        return Err(bad_gateway("too large"));
    }
    let bytes = Limited::new(response.into_body(), MAX_BYTES).collect().await.map_err(|_| bad_gateway("too large"))?.to_bytes();
    Ok(served(bytes, content_type, None))
}

/// Whether an address is on the public internet. Everything else is refused: loopback, private, CGNAT,
/// link-local, multicast, documentation and reserved ranges, and IPv6 forms that embed one of those.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_v4(v4); // ::ffff:a.b.c.d
            }
            let s = ip.segments();
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let octets = ip.octets();
                return is_public_v4(Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15])); // NAT64 64:ff9b::/96
            }
            s[0] & 0xe000 == 0x2000 // global unicast 2000::/3, which leaves out ::, ::1, fc00::/7, fe80::/10, ff00::/8
                && !(s[0] == 0x2001 && s[1] < 0x0200) // 2001::/23 protocol assignments: Teredo, ORCHID, benchmarking
                && !(s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                && s[0] != 0x2002 // 6to4, which embeds an IPv4 address
                && !(s[0] == 0x3fff && s[1] < 0x1000) // documentation 3fff::/20
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // "this network"
        || a == 10
        || a == 127
        || a >= 224 // multicast, reserved, broadcast
        || (a == 100 && (64..128).contains(&b)) // CGNAT
        || (a == 169 && b == 254) // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b)) // private, Docker's default pools
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // IETF assignments, documentation
        || (a == 192 && b == 88 && c == 99) // 6to4 relay
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}
