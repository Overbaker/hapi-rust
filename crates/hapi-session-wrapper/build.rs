use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn main() {
    for key in ["HAPI_WRAPPER_GIT_SHA", "GITHUB_SHA"] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(reference) =
        git(&["symbolic-ref", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r]))
    {
        println!("cargo:rerun-if-changed={reference}");
    }
    let sha = std::env::var("HAPI_WRAPPER_GIT_SHA")
        .ok()
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HAPI_BUILD_REVISION={sha}");
}
