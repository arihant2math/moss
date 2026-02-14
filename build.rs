use std::env;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::macros::format_description;

fn build_modules(linker_script: &Path) {
    println!("cargo::rerun-if-changed=modules/");
    let linker_opt = format!("-Wl,-T,{}", linker_script.display());
    cc::Build::new()
        .compiler(Path::new("aarch64-none-elf-gcc"))
        .file("./modules/simple-c-module/simple-module.c")
        .flags(&["-Imodules/include/", &linker_opt])
        .warnings(false)
        .compile("simple-c-module");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-arg=--whole-archive");
    println!("cargo:rustc-link-lib=static=simple-c-module");
    println!("cargo:rustc-link-arg=--no-whole-archive");
}


fn main() {
    let linker_script = match env::var("CARGO_CFG_TARGET_ARCH") {
        Ok(arch) if arch == "aarch64" => PathBuf::from("./src/arch/arm64/boot/linker.ld"),
        Ok(arch) => {
            println!("Unsupported arch: {arch}");
            std::process::exit(1);
        }
        Err(_) => unreachable!("Cargo should always set the arch"),
    };

    println!("cargo::rerun-if-changed={}", linker_script.display());
    println!("cargo::rustc-link-arg=-T{}", linker_script.display());

    // Build modules
    build_modules(&linker_script);

    // Set an environment variable with the date and time of the build
    let now = OffsetDateTime::now_utc();
    let format = format_description!(
        "[weekday repr:short] [month repr:short] [day] [hour]:[minute]:[second] UTC [year]"
    );
    let timestamp = now.format(&format).unwrap();
    #[cfg(feature = "smp")]
    println!("cargo:rustc-env=MOSS_VERSION=#1 Moss SMP {timestamp}");
    #[cfg(not(feature = "smp"))]
    println!("cargo:rustc-env=MOSS_VERSION=#1 Moss {timestamp}");
}
