//! Stamps build identity into the binary so "which build is actually running?"
//! is answerable from the boot log instead of by guesswork.
//!
//! Three separate incidents traced to a stale artifact before this existed: a
//! six-week-old `main` deployed to the host (symptom: one missing log field), a
//! deploy where `cargo` was not on PATH so the old binary was silently restarted
//! (symptom: none, the boot line looked perfect), and a client running a
//! four-day-old exe from a different folder (symptom: a command that "did not
//! fire"). All three cost hours. This costs microseconds.

use std::process::Command;

fn main() {
    // Short commit hash, plus a `-dirty` marker when the tree has uncommitted
    // changes — a dev-box build is not the same artifact as the commit it claims.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let commit = if dirty { format!("{hash}-dirty") } else { hash };
    println!("cargo:rustc-env=PD_BUILD_COMMIT={commit}");

    // Rerun when HEAD moves, so the stamp cannot go stale within a checkout.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
