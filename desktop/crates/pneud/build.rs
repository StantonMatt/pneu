fn main() {
    println!("cargo:rustc-check-cfg=cfg(pneu_qt_drag)");
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rerun-if-env-changed=SLINT_NO_QT");
        println!("cargo:rerun-if-changed=src/ui/linux_drag.rs");
        println!("cargo:rerun-if-changed=src/ui/linux_drag.cpp");
        let version = std::env::var("DEP_QT_VERSION").unwrap_or_default();
        // Match Slint's Qt availability gate, including its winit-only builds.
        if std::env::var_os("SLINT_NO_QT").is_none()
            && std::env::var("DEP_QT_FOUND").is_ok_and(|v| v == "1")
            && (version.starts_with("5.15") || version.starts_with("6."))
        {
            let mut config = cpp_build::Config::new();
            for flag in std::env::var("DEP_QT_COMPILE_FLAGS")
                .expect("Qt compiler flags")
                .split_terminator(';')
            {
                config.flag(flag);
            }
            config.flag_if_supported("-std=c++17");
            config
                .include(std::env::var("DEP_QT_INCLUDE_PATH").expect("Qt headers"))
                .include("src/ui")
                .build("src/ui/linux_drag.rs");
            println!("cargo:rustc-cfg=pneu_qt_drag");
        }
    }
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
