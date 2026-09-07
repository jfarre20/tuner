//! Windows: embed the icon and copy FFmpeg runtime DLLs next to the exe. All platforms: the `d3d`
//! cfg selects the Direct3D 11 presenter (Windows without the wgpu-render feature).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(d3d)");
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_ge_5)");
    // FFmpeg 5+ (libavformat 59+) has avformat_index_get_entry; 4.x exposes the index array.
    // Windows builds ship a 6.x FFMPEG_DIR; elsewhere ask pkg-config.
    let lavf_major = if cfg!(windows) {
        59
    } else {
        std::process::Command::new("pkg-config")
            .args(["--modversion", "libavformat"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|v| v.trim().split('.').next()?.parse::<u32>().ok())
            .unwrap_or(59)
    };
    if lavf_major >= 59 {
        println!("cargo:rustc-cfg=ffmpeg_ge_5");
    }
    if cfg!(windows) && std::env::var_os("CARGO_FEATURE_WGPU_RENDER").is_none() {
        println!("cargo:rustc-cfg=d3d");
    }
    #[cfg(windows)]
    {
        use std::{env, fs, path::PathBuf};
        // Exe icon for every binary (needs rc.exe from the Windows SDK, which the MSVC toolchain has).
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico").set("ProductName", "TUNER").set("FileDescription", "TUNER - a 90s cable box for your video folder");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon resource not embedded: {e}");
        }
        println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
        let Ok(ffdir) = env::var("FFMPEG_DIR") else { return };
        let out = PathBuf::from(env::var("OUT_DIR").unwrap());
        let target_dir = out.ancestors().nth(3).unwrap().to_path_buf(); // target/<profile>
        for e in fs::read_dir(PathBuf::from(ffdir).join("bin")).unwrap().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "dll") {
                let dst = target_dir.join(p.file_name().unwrap());
                if !dst.exists() {
                    fs::copy(&p, &dst).unwrap();
                }
            }
        }
    }
}
