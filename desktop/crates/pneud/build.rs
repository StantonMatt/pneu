fn main() {
    println!("cargo:rerun-if-changed=src/ui/macos/camera.m");
    #[cfg(target_os = "macos")]
    {
        cc::Build::new()
            .file("src/ui/macos/camera.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .flag("-mmacosx-version-min=15.4")
            .warnings_into_errors(true)
            .compile("pneu_camera");
        for framework in [
            "Foundation",
            "AVFoundation",
            "Vision",
            "CoreMedia",
            "CoreVideo",
        ] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
