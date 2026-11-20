use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, handshake::client::Request, Error,
};

#[cfg(unix)]
pub type Address = std::path::Path;
#[cfg(unix)]
pub type Endpoint = std::path::PathBuf;
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;

#[cfg(windows)]
#[derive(Clone)]
pub struct Endpoint {
    pub addr: std::net::SocketAddr,
    pub token: String,
}
#[cfg(windows)]
pub type Address = Endpoint;
#[cfg(windows)]
pub type Stream = tokio::net::TcpStream;

#[cfg(windows)]
pub fn token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub async fn stream(address: &Address) -> std::io::Result<Stream> {
    #[cfg(unix)]
    return Stream::connect(address).await;
    #[cfg(windows)]
    return Stream::connect(address.addr).await;
}

pub fn request(address: &Address, token: Option<&str>) -> Result<Request, Error> {
    #[cfg(unix)]
    let mut request = {
        let _ = address;
        "ws://localhost/".into_client_request()?
    };
    #[cfg(windows)]
    let mut request = format!("ws://{}/", address.addr).into_client_request()?;
    #[cfg(windows)]
    let token = token.or(Some(address.token.as_str()));
    if let Some(token) = token.filter(|s| !s.is_empty()) {
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse()?);
    }
    Ok(request)
}
