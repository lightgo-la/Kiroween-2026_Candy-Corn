//! Windows exe 파일 아이콘과 버전 정보를 리소스로 넣는다.
//! 아이콘 원본: pumpkin_premium.png -> assets/icon.ico (16~256px 다중 크기)

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "Sugar-Coat Studio");
    res.set("FileDescription", "Sugar-Coat Studio");
    // rc.exe(Windows SDK)가 없는 PC에서도 빌드는 되도록 실패는 경고로만 남긴다.
    if let Err(e) = res.compile() {
        println!("cargo:warning=exe 아이콘을 넣지 못했습니다 (Windows SDK의 rc.exe 필요): {e}");
    }
}
