fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../packaging/windows/Pneu.ico");
        println!("cargo:rerun-if-changed=../../packaging/windows/Pneu.exe.manifest");
        winresource::WindowsResource::new()
            .set_icon("../../packaging/windows/Pneu.ico")
            .set_manifest_file("../../packaging/windows/Pneu.exe.manifest")
            .set("FileDescription", "Pneu")
            .set("ProductName", "Pneu")
            .set("OriginalFilename", "Pneu.exe")
            .compile()
            .expect("compile Windows icon, version and application manifest resources");
    }
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
