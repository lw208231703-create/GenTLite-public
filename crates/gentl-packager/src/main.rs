//! `cargo cti` — build the workspace and place a `.cti` twin next to each
//! producer shared library in `target/<profile>/`, so the directory is
//! directly usable as a `GENICAM_GENTL64_PATH` entry.
//!
//! A `.cti` is the same shared library under the extension GenTL consumers
//! look for: `gentl_gige.dll` / `gentl_u3v.dll` on Windows, and
//! `libgentl_gige.so` / `libgentl_u3v.so` on Unix.

use std::path::{Path, PathBuf};
use std::process::Command;

const PRODUCERS: &[&str] = &["gentl_gige", "gentl_u3v"];

fn main() {
    // Workspace root: this crate sits under <root>/crates/.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("packager crate is crates/gentl-packager")
        .to_path_buf();

    // Same profile this binary itself was built with (release for
    // `cargo cti`, dev for `cargo ctid`).
    let exe = std::env::current_exe().expect("current_exe");
    let profile = exe
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "release".into());

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let profile_dir = target_dir.join(&profile);

    // 1) Build the producers.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(&root)
        .arg("build")
        .arg("-p")
        .arg("gentl-gige")
        .arg("-p")
        .arg("gentl-u3v");
    if profile == "release" {
        cmd.arg("--release");
    } else if profile != "debug" {
        cmd.arg("--profile").arg(&profile);
    }
    println!("packager: {} (in {})", format_cmd(&cmd), root.display());
    let st = cmd.status().expect("spawn cargo build");
    if !st.success() {
        eprintln!("packager: cargo build failed ({st})");
        std::process::exit(1);
    }

    // 2) Copy each producer shared library next to its .cti twin.
    let mut ok = true;
    for name in PRODUCERS {
        let suffix = if std::env::consts::DLL_SUFFIX.starts_with('.') {
            std::env::consts::DLL_SUFFIX.to_owned()
        } else {
            format!(".{}", std::env::consts::DLL_SUFFIX)
        };
        let library = profile_dir.join(format!(
            "{}{}{}",
            std::env::consts::DLL_PREFIX,
            name,
            suffix
        ));
        let cti = profile_dir.join(format!("{name}.cti"));
        match std::fs::copy(&library, &cti) {
            Ok(_) => println!("packager: {} -> {}", library.display(), cti.display()),
            Err(e) => {
                eprintln!(
                    "packager: copy {} -> {} failed: {e}",
                    library.display(),
                    cti.display()
                );
                ok = false;
            }
        }
    }
    if ok {
        println!("packager: done. Add {} to GENICAM_GENTL64_PATH", profile_dir.display());
    } else {
        std::process::exit(1);
    }
}

fn format_cmd(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}
