use super::client::{Delivery, SessionClient};
use std::io;
impl SessionClient {
    pub(super) async fn backfill(&self, start_cursor: Option<i64>) -> io::Result<()> {
        let _guard = self.backfill_lock.lock().await;
        let Some(mut cursor) = start_cursor else {
            return Ok(());
        };
        if let Some(checkpoint) = self.filter.lock().unwrap().backfill_cursor() {
            cursor = cursor.max(checkpoint);
        }
        loop {
            let rows = self.rest.messages(&self.id, cursor).await?;
            let mut next = cursor;
            for row in &rows {
                next = next.max(row["seq"].as_i64().unwrap_or(cursor));
                self.incoming_message(row, Delivery::Backfill);
            }
            self.filter.lock().unwrap().page_scanned(next);
            if rows.len() < 200 || next <= cursor {
                return Ok(());
            }
            cursor = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, rest::Rest};
    use serde_json::json;
    use std::time::Duration;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    #[tokio::test]
    async fn waiting_backfill_executes_after_lock_release() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            home: Default::default(),
            api_url: format!("http://{}", listener.local_addr().unwrap()),
            cli_api_token: "token".into(),
            extra_headers: vec![],
            machine_id: None,
        };
        let rest = Rest::new(&config).await.unwrap();
        config.api_url = "http://127.0.0.1:0".into();
        let (client, _events) = SessionClient::start(
            &config,
            rest,
            &json!({"id":"s","metadataVersion":0,"agentStateVersion":0}),
        )
        .unwrap();
        let old_holder = client.backfill_lock.lock().await;
        let mut next = Box::pin(client.backfill(Some(19)));
        assert!(futures_util::FutureExt::now_or_never(next.as_mut()).is_none());
        drop(old_holder);
        let serve = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream).lines();
            assert_eq!(
                reader.next_line().await.unwrap().unwrap(),
                "GET /cli/sessions/s/messages?afterSeq=19&limit=200 HTTP/1.1"
            );
            while !reader.next_line().await.unwrap().unwrap().is_empty() {}
            stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 15\r\n\r\n{\"messages\":[]}").await.unwrap();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(next, serve) })
                .await
                .unwrap();
        result.unwrap();
        client.close();
    }
}
