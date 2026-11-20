use super::super::tests::Fixture;
use super::*;
use std::os::unix::fs::symlink;
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
            .env("HAPI_MAX_UPLOAD_BYTES", " 2097152 ")
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
            cleanup_upload_dir(session);
        });
}
