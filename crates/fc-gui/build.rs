//! Windows only: embed the application manifest (long-path aware, per-monitor
//! DPI, UTF-8 code page) and version resources. No-op on other targets.

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/windows/fcopy.rc");
    println!("cargo:rerun-if-changed=../../packaging/windows/fcopy.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../packaging/windows/fcopy.rc", embed_resource::NONE)
            .manifest_required()
            .expect("embedding the Windows manifest failed (is rc.exe / windres installed?)");
    }
}
