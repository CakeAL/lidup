fn main() {
    // Link Apple's display frameworks. The private SkyLight symbol is loaded at
    // runtime via dlopen/dlsym from libloading in displays.rs.
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-cfg=macos");
}
