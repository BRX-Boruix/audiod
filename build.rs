//! 用户态程序链接脚本注入。
//!
//! 通过 `CARGO_MANIFEST_DIR` 得到绝对路径，把 `linker.ld` 传给链接器，
//! 将 `.text` 等段定位到用户态地址（0x400000 起），`ENTRY(_start)`。
//!
//! `-no-pie`：强制生成 ET_EXEC（非 PIE）。rust-lld 默认生成 PIE（ET_DYN），
//! 而内核 ELF 加载器雏形只接受 ET_EXEC；裸机静态程序无需重定位，故关闭 PIE。
//! 缺此注入会把段链接到 vaddr 0x0（与内核用户半区冲突），加载器无法装载。

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
        println!("cargo:rustc-link-arg=-no-pie");
    }
}
