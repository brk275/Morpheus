//! Archive identities from the pinned upstream release.
// Keep WEBRTC_TAG and these identities reviewed together.

pub(super) const TAG: &str = "webrtc-51ef663";

pub(super) struct Pin {
    pub bytes: u64,
    pub sha256: &'static str,
}

pub(super) fn for_triple(triple: &str) -> Option<Pin> {
    let (bytes, sha256) = match triple {
        "android-arm-release" => (
            154748853,
            "7814aadbf0502d0b92258460b0b0fcc01af936c3767956fdb9f1970e8685334a",
        ),
        "android-arm64-release" => (
            155977458,
            "1ba0acad6745aea3054f0dd5a97257cd4d2fe6ac763c00197299152ec38699ba",
        ),
        "android-x64-release" => (
            157745928,
            "e50e70d5991365304bbc457724289a393714e708cd5ce1dfa8f74375ba643001",
        ),
        "ios-device-arm64-release" => (
            218533711,
            "797e99ad58ae758d9a1187135d704e3a3d6e1e9877cb4c8e834d809d7f7e6208",
        ),
        "ios-simulator-arm64-release" => (
            219184242,
            "9663e560682ee5d0a4119328b9c4bae77541a6bbf8c516adc3d60ab7ea5cde94",
        ),
        "linux-arm64-release" => (
            165526092,
            "b55ca62c28b18dc5bb720a7082ae468d78f213ca39bcccf84195f2fe58cfe713",
        ),
        "linux-x64-release" => (
            149683670,
            "dce9d9414cd8e2cc9a471c054481851b257ad2a879112ce5be56369b4fefdaed",
        ),
        "mac-arm64-release" => (
            265849959,
            "f9be49b9fee9cd1588da8a40c85bda21f5ea49c4ed82662dc889b1c919569a08",
        ),
        "mac-x64-release" => (
            223690324,
            "171f1ba0866e5947e0d420ecc1c33814476e1919131b81bb87c8729d45d72127",
        ),
        "win-arm64-release" => (
            108193988,
            "929bd9e5682549eb3d43104e59c6c561f3d429f29caefea1c227b94839e7c5b7",
        ),
        "win-x64-release" => (
            110167699,
            "0a56a5c91b3b7b4222082b8a09388d16a802cde22d67295beb24c55700755b30",
        ),
        _ => return None,
    };
    Some(Pin { bytes, sha256 })
}
