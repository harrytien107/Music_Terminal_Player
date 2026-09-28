use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/music-terminal-player.ico");
    println!("cargo:rerun-if-changed=assets/windows.rc");

    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let resource = out_dir.join("music-terminal-player.res");
    let status = Command::new("rc.exe")
        .arg("/nologo")
        .arg(format!("/fo{}", resource.display()))
        .arg("assets/windows.rc")
        .status()
        .expect("failed to start rc.exe; build from the Visual Studio developer environment");

    assert!(
        status.success(),
        "rc.exe failed to compile the Windows resources"
    );
    println!("cargo:rustc-link-arg={}", resource.display());
}
