//! Optional TLS on the existing TCP port. Peer addresses are never taken from headers.
use anyhow::{Context, Result, bail};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll, ready},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
};

pub fn configuration(config: &crate::config::Config) -> Result<Option<Arc<rustls::ServerConfig>>> {
    let (cert, key) = match (&config.tls_cert, &config.tls_key) {
        (None, None) if !config.https_only => return Ok(None),
        (Some(cert), Some(key)) if cert.is_absolute() && key.is_absolute() => (cert, key),
        _ => bail!("TLS証明書と秘密鍵を絶対パスで両方指定してください。"),
    };
    let certificates = CertificateDer::pem_file_iter(cert)
        .context("TLSの公開証明書を読み取れません。")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("TLSの公開証明書を解析できません。")?;
    if certificates.is_empty() {
        bail!("TLSの証明書チェーンが空です。")
    }
    let private = PrivateKeyDer::from_pem_file(key)
        .context("TLS秘密鍵を読み取れません。配置と実行者のアクセス権を確認してください。")?;
    let mut server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private)
        .context("TLS証明書と秘密鍵の組合せを確認してください。")?;
    // Preserve the existing HTTP/1.1 Host-header authorization contract.
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Some(Arc::new(server)))
}

/// Only advertise addresses covered by the configured public certificate's SAN.
pub fn covers_host(path: &std::path::Path, host: &str) -> bool {
    let Ok(mut certificates) = CertificateDer::pem_file_iter(path) else {
        return false;
    };
    let Some(Ok(cert)) = certificates.next() else {
        return false;
    };
    let Ok(cert) = webpki::EndEntityCert::try_from(&cert) else {
        return false;
    };
    let Ok(name) = rustls::pki_types::ServerName::try_from(host) else {
        return false;
    };
    cert.verify_is_valid_for_subject_name(&name).is_ok()
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type Handshake = Pin<Box<dyn Future<Output = io::Result<Box<dyn Stream>>> + Send>>;

/// Delay sniffing/handshake until this connection is polled, not in the accept loop.
pub struct Connection {
    pending: Option<Handshake>,
    stream: Option<Box<dyn Stream>>,
}
impl Connection {
    fn new(stream: TcpStream, tls: Option<Arc<rustls::ServerConfig>>, strict: bool) -> Self {
        let handshake = async move {
            let Some(tls) = tls else {
                return Ok(Box::new(stream) as Box<dyn Stream>);
            };
            let setup = async move {
                let mut first = [0u8; 1];
                if stream.peek(&mut first).await? == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                if first[0] == 0x16 {
                    // A failed TLS handshake is never retried as plaintext HTTP.
                    let secured = TlsAcceptor::from(tls).accept(stream).await?;
                    Ok(Box::new(secured) as Box<dyn Stream>)
                } else if strict {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "HTTPS is required",
                    ))
                } else {
                    Ok(Box::new(stream) as Box<dyn Stream>)
                }
            };
            tokio::time::timeout(Duration::from_secs(10), setup)
                .await
                .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
        };
        Self {
            pending: Some(Box::pin(handshake)),
            stream: None,
        }
    }
    fn poll_stream(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<&mut Box<dyn Stream>>> {
        if let Some(pending) = self.pending.as_mut() {
            let result = ready!(pending.as_mut().poll(cx));
            self.pending = None;
            self.stream = Some(result?);
        }
        Poll::Ready(
            self.stream
                .as_mut()
                .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionAborted)),
        )
    }
}
impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let stream = ready!(self.poll_stream(cx))?;
        Pin::new(stream.as_mut()).poll_read(cx, buffer)
    }
}
impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let stream = ready!(self.poll_stream(cx))?;
        Pin::new(stream.as_mut()).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let stream = ready!(self.poll_stream(cx))?;
        Pin::new(stream.as_mut()).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if let Some(stream) = self.stream.as_mut() {
            Pin::new(stream.as_mut()).poll_shutdown(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

pub struct Listener {
    inner: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    strict: bool,
}
impl Listener {
    pub fn new(inner: TcpListener, tls: Option<Arc<rustls::ServerConfig>>, strict: bool) -> Self {
        Self { inner, tls, strict }
    }
}
impl axum::serve::Listener for Listener {
    type Io = Connection;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Connection, SocketAddr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, peer)) => {
                    return (Connection::new(stream, self.tls.clone(), self.strict), peer);
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tls_is_opt_in_and_partial_or_relative_configuration_is_rejected() {
        let mut config = crate::config::Config::default();
        assert!(configuration(&config).unwrap().is_none());
        config.https_only = true;
        assert!(configuration(&config).is_err());
        config.tls_cert = Some("cert.pem".into());
        config.tls_key = Some("key.pem".into());
        assert!(configuration(&config).is_err());
    }
}
