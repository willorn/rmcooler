fn main() {
    // 把 Info.plist 嵌进主程序二进制（状态栏 app 需要它声明权限）
    // CARGO_MANIFEST_DIR 跟着项目走，移动目录后无需改动
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-sectcreate");
    println!("cargo:rustc-link-arg=__TEXT");
    println!("cargo:rustc-link-arg=__info_plist");
    println!("cargo:rustc-link-arg={manifest}/Info.plist");
    println!("cargo:rerun-if-changed=Info.plist");
}
