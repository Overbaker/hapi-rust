use bytes::Bytes;
use futures_util::Future;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{body::Incoming, server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use std::{convert::Infallible, io};
use tokio::{
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};

pub(crate) type Reply = Response<BoxBody<Bytes, Infallible>>;
pub(crate) fn reply(status: u16, mime: &str, data: impl Into<Bytes>) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", mime)
        .body(Full::new(data.into()).boxed())
        .unwrap()
}
pub struct Listener {
    pub port: u16,
    task: Option<JoinHandle<()>>,
    stop: tokio::sync::watch::Sender<bool>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl Listener {
    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
    pub(crate) async fn bind<F, Fut>(handler: F) -> io::Result<Self>
    where
        F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _=stopped.changed()=>break,
                    incoming=listener.accept()=> {
                        let Ok((stream,_))=incoming else {break};
                        let handler=handler.clone();
                        let mut closing=stopped.clone();
                        connections.spawn(async move {
                            let service=service_fn(move |r| { let response=handler(r); async move {Ok::<_, Infallible>(response.await)} });
                            let conn=http1::Builder::new().serve_connection(TokioIo::new(stream),service);
                            tokio::pin!(conn);
                            tokio::select!{_= &mut conn=>{},_=closing.changed()=>{conn.as_mut().graceful_shutdown();let _=conn.await;}}

                        });
                    }
                    _=connections.join_next(), if !connections.is_empty()=>{}
                }
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while connections.join_next().await.is_some() {}
            })
            .await;
        });
        Ok(Self {
            port,
            task: Some(task),
            stop,
        })
    }
}
