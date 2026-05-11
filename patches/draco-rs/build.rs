fn compile() -> String {
    let mut cfg = cmake::Config::new("draco");
    cfg.define("CMAKE_BUILD_TYPE", "Release")
        .define("DRACO_POINT_CLOUD_COMPRESSION", "ON")
        .define("DRACO_MESH_COMPRESSION", "ON")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("BUILD_SHARED_LIBS", "OFF");

    let is_msvc = std::env::var("CARGO_CFG_TARGET_ENV")
        .map(|e| e == "msvc")
        .unwrap_or(false);

    if is_msvc {
        // MSVC: /w disables all warnings (equivalent to -w)
        cfg.cxxflag("/w");
    } else {
        // GCC/Clang: -fPIC for shared lib compat, silence warnings
        cfg.cxxflag("-fPIC")
            .cxxflag("-w")
            .cxxflag("-Wno-everything");
    }

    let dst = cfg.build();

    dst.display().to_string()
}

fn generate_bindings(out_dir: String) -> miette::Result<()> {
    let includes = vec![
        "src".to_string(),
        "draco/src".to_string(),
        format!("{}/include", out_dir),
    ];

    let mut b = autocxx_build::Builder::new("src/bindgen.rs", &includes)
        .extra_clang_args(&[
            "-std=c++14",
            "-w", // silences all warnings during clang parsing
            "-Wno-everything",
        ])
        .build()?;

    b.opt_level(3)
        .cpp(true)
        .std("c++14")
        // .flag("-ldraco")
        // .flag("-Wl,-l:libdraco.a")
        // .flag(format!("-L{}", out_dir))
        // .flag(format!("-L{}/build", out_dir))
        .compile("draco-rs");

    println!("cargo:rerun-if-changed=src/bindgen.rs");
    println!("cargo:rerun-if-changed=src/extra.h");

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=draco-rs");

    println!("cargo:rustc-link-search=native={}/lib", out_dir);
    println!("cargo:rustc-link-search=native={}/lib64", out_dir);
    println!("cargo:rustc-link-lib=static=draco");

    Ok(())
}

fn main() -> miette::Result<()> {
    let out_dir = compile();

    generate_bindings(out_dir)?;
    Ok(())
}
