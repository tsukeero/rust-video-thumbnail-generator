use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // Only copy DLLs on Windows
    if cfg!(target_os = "windows") {
        if let Ok(ffmpeg_dir) = env::var("FFMPEG_DIR") {
            let ffmpeg_bin = PathBuf::from(ffmpeg_dir).join("bin");

            if ffmpeg_bin.exists() {
                let out_dir = env::var("OUT_DIR").unwrap();
                let target_dir = PathBuf::from(out_dir)
                    .ancestors()
                    .nth(3)
                    .unwrap()
                    .to_path_buf();

                println!("cargo:rerun-if-changed={}", ffmpeg_bin.display());

                // Copy all DLL files from FFMPEG_DIR/bin to target directory
                if let Ok(entries) = fs::read_dir(&ffmpeg_bin) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("dll") {
                            let file_name = path.file_name().unwrap();
                            let dest = target_dir.join(file_name);
                            if let Err(e) = fs::copy(&path, &dest) {
                                println!("cargo:warning=Failed to copy {}: {}", path.display(), e);
                            } else {
                                println!("cargo:info=Copied {} to {}", path.display(), dest.display());
                            }
                        }
                    }
                }
            } else {
                println!("cargo:warning=FFMPEG_DIR set but bin directory not found: {}", ffmpeg_bin.display());
            }
        } else {
            println!("cargo:warning=FFMPEG_DIR not set. FFmpeg DLLs will not be copied automatically.");
        }
    }
}
