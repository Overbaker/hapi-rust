use super::super::tests::Fixture;
use super::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(unix)]
use tempfile::tempdir;

#[test]
fn upload_rpc_conflicts_and_delete_boundaries() {
    // Isolate the process-wide slot limit and exit cleanup from upload_lifecycle.
    let marker = "HAPI_TEST_UPLOAD_CONFLICT";
    if std::env::var_os(marker).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rpc::handlers::uploads::tests_uploads::upload_rpc_conflicts_and_delete_boundaries",
                "--nocapture",
            ])
            .env(marker, "1")
            .env("HAPI_MAX_UPLOAD_BYTES", "\u{feff} 2097152 ")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let f = Fixture::new();
            register(&f.rpc);
            let session = "rpc-overlap";
            assert_eq!(
                f.call(
                    "uploadInit",
                    json!({"sessionId":session,"filename":"large","size":(3<<20)})
                )
                .await,
                upload_error("upload_too_large", "File too large (max 2MB)")
            );
            let single = f
                .call(
                    "uploadFile",
                    json!({"sessionId":session,"filename":"example","content":"YQ=="}),
                )
                .await;
            let path = Path::new(single["path"].as_str().unwrap());
            for path in [
                PathBuf::from("/etc/passwd"),
                path.parent().unwrap().join("../x"),
            ] {
                assert_eq!(
                    f.call("deleteUpload", json!({"sessionId":session,"path":path}))
                        .await,
                    error("Invalid upload path")
                );
            }
            #[cfg(unix)]
            {
                let outside = tempdir().unwrap();
                let old = outside.path().join("file");
                fs::write(&old, "kept").unwrap();
                let link = path.parent().unwrap().join("linked");
                symlink(outside.path(), &link).unwrap();
                assert_eq!(
                    f.call(
                        "deleteUpload",
                        json!({"sessionId":session,"path":link.join("file")})
                    )
                    .await,
                    error("Invalid upload path")
                );
                assert!(old.exists());
            }
            let init = f
                .call(
                    "uploadInit",
                    json!({"sessionId":session,"filename":"chunk","size":1}),
                )
                .await;
            assert_eq!(init["maxUploadBytes"], 2 * MIB);
            let id = &init["uploadId"];
            let (release, hold) = tokio::sync::oneshot::channel::<()>();
            let block = tokio::task::spawn_blocking(move || hold.blocking_recv());
            let chunk = f.call(
                "uploadChunk",
                json!({"sessionId":session,"uploadId":id,"index":0,"content":"YQ=="}),
            );
            tokio::pin!(chunk);
            assert!(futures_util::FutureExt::now_or_never(chunk.as_mut()).is_none());
            for m in ["uploadChunk", "uploadFinalize"] {
                assert_eq!(
                    f.call(m, json!({"uploadId":id,"sessionId":session})).await["code"],
                    "upload_conflict"
                );
                assert_eq!(
                    f.call(m, json!({"uploadId":id,"sessionId":"other"})).await["code"],
                    "upload_not_found"
                );
                assert_eq!(
                    f.call(m, json!({"sessionId":session})).await["error"],
                    "uploadId is required"
                );
            }
            release.send(()).unwrap();
            block.await.unwrap().unwrap();
            assert_eq!(chunk.await["receivedBytes"], 1);
            assert_eq!(
                f.call("uploadFinalize", json!({"sessionId":session,"uploadId":id}))
                    .await["success"],
                true
            );
            let init = f
                .call(
                    "uploadInit",
                    json!({"sessionId":session,"filename":"aborted","size":1}),
                )
                .await;
            let id = &init["uploadId"];
            f.call(
                "uploadChunk",
                json!({"sessionId":session,"uploadId":id,"index":0,"content":"YQ=="}),
            )
            .await;
            let (release, hold) = tokio::sync::oneshot::channel::<()>();
            let block = tokio::task::spawn_blocking(move || hold.blocking_recv());
            let finalize = f.call("uploadFinalize", json!({"sessionId":session,"uploadId":id}));
            tokio::pin!(finalize);
            assert!(futures_util::FutureExt::now_or_never(finalize.as_mut()).is_none());
            let abort = f.call("uploadAbort", json!({"sessionId":session,"uploadId":id}));
            tokio::pin!(abort);
            assert!(futures_util::FutureExt::now_or_never(abort.as_mut()).is_none());
            release.send(()).unwrap();
            block.await.unwrap().unwrap();
            assert_eq!(
                finalize.await,
                upload_error("upload_conflict", "Upload aborted during finalization")
            );
            assert_eq!(abort.await["success"], true);
            let dir = path.parent().unwrap();
            assert!(!dir
                .read_dir()
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("aborted")));
            cleanup_upload_dir(session);
            let queued = "rpc-queued";
            let (release, hold) = tokio::sync::oneshot::channel::<()>();
            let block = tokio::task::spawn_blocking(move || hold.blocking_recv());
            let file = f.call(
                "uploadFile",
                json!({"sessionId":queued,"filename":"late","content":"YQ=="}),
            );
            tokio::pin!(file);
            assert!(futures_util::FutureExt::now_or_never(file.as_mut()).is_none());
            let init = f.call(
                "uploadInit",
                json!({"sessionId":queued,"filename":"late","size":1}),
            );
            tokio::pin!(init);
            assert!(futures_util::FutureExt::now_or_never(init.as_mut()).is_none());
            cleanup_upload_dir(queued);
            release.send(()).unwrap();
            block.await.unwrap().unwrap();
            assert_eq!(file.await, error("Upload directory cleanup requested"));
            assert_eq!(init.await, error("Upload directory cleanup requested"));
            assert!(!STATE.lock().unwrap().dirs.contains_key(queued));
            assert_eq!(
                f.call(
                    "uploadFile",
                    json!({"sessionId":queued,"filename":"next","content":"YQ=="}),
                )
                .await["success"],
                true
            );
            cleanup_upload_dir(queued);
        });
}
