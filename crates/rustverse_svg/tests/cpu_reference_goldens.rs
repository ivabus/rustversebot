use std::path::{Path, PathBuf};

use rustverse_svg::{RenderScale, TopDA};
use serde_json::Value;

const MANIFEST: &str = include_str!("goldens/manifest.json");

#[test]
fn cpu_reference_goldens_are_current_and_self_describing() {
    let manifest: Value =
        serde_json::from_str(MANIFEST).expect("golden manifest must be valid JSON");
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["hash_algorithm"], "sha256");

    let fixtures = manifest["fixtures"]
        .as_array()
        .expect("manifest fixtures must be an array");
    assert!(
        !fixtures.is_empty(),
        "manifest must select at least one fixture"
    );

    let names = fixtures
        .iter()
        .map(|fixture| required_str(fixture, "name"))
        .collect::<Vec<_>>();
    let mut sorted_names = names.clone();
    sorted_names.sort_unstable();
    assert_eq!(names, sorted_names, "fixtures must be sorted by name");

    for fixture in fixtures {
        verify_fixture(fixture);
    }
}

fn verify_fixture(fixture: &Value) {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tests_dir = crate_dir.join("tests");
    let name = required_str(fixture, "name");
    let input_path = tests_dir.join(required_str(fixture, "input"));
    let golden_path = tests_dir.join(required_str(fixture, "golden"));

    let input = std::fs::read(&input_path).expect("fixture input must be readable");
    assert_hash(&input_path, &input, required_str(fixture, "input_sha256"));

    let data: TopDA = serde_json::from_slice(&input).expect("fixture input must deserialize");
    let actual =
        rustverse_svg::try_render_from_serialize_with_scale("top_da.j2", &data, RenderScale::ONE)
            .expect("CPU reference fixture must render");

    if std::env::var_os("UPDATE_RENDER_GOLDENS").as_deref() == Some("1".as_ref()) {
        std::fs::write(&golden_path, &actual).expect("updated golden must be writable");
        eprintln!(
            "updated {name}; set golden_sha256 to {}",
            hex_sha256(&actual)
        );
        return;
    }

    let expected = std::fs::read(&golden_path)
        .expect("golden must exist; regenerate explicitly with UPDATE_RENDER_GOLDENS=1");
    assert_hash(
        &golden_path,
        &expected,
        required_str(fixture, "golden_sha256"),
    );

    let (width, height) = png_dimensions(&actual);
    assert_eq!(
        (width, height),
        (
            required_u32(&fixture["logical_size"], "width"),
            required_u32(&fixture["logical_size"], "height"),
        ),
        "{name} must render at its logical size when scale is 1.0"
    );
    assert_eq!(fixture["scale"].as_f64(), Some(1.0));
    assert_eq!(
        fixture["comparison_policy"]["max_channel_delta"].as_u64(),
        Some(0)
    );
    assert_eq!(
        fixture["comparison_policy"]["max_differing_pixels"].as_u64(),
        Some(0)
    );
    assert_eq!(actual, expected, "{name} CPU reference golden changed");

    verify_resource_hashes(&crate_dir, &fixture["renderer"]["sources"]);
    verify_resource_hashes(&crate_dir, &Value::Array(vec![fixture["font"].clone()]));
    verify_resource_hashes(&crate_dir, &fixture["images"]);
}

fn verify_resource_hashes(crate_dir: &Path, resources: &Value) {
    for resource in resources
        .as_array()
        .expect("hashed resources must be an array")
    {
        let path = crate_dir.join("tests").join(required_str(resource, "path"));
        let bytes = std::fs::read(&path).expect("manifest resource must be readable");
        assert_hash(&path, &bytes, required_str(resource, "sha256"));
    }
}

fn required_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string"))
}

fn required_u32(value: &Value, key: &str) -> u32 {
    value[key]
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .unwrap_or_else(|| panic!("{key} must be a u32"))
}

fn assert_hash(path: &Path, bytes: &[u8], expected: &str) {
    assert_eq!(
        hex_sha256(bytes),
        expected,
        "{} hash changed",
        path.display()
    );
}

fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "expected PNG bytes"
    );
    assert_eq!(&bytes[12..16], b"IHDR", "PNG must start with IHDR");
    (
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    )
}

fn hex_sha256(bytes: &[u8]) -> String {
    sha256(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in words[..16].iter_mut().enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes(chunk[start..start + 4].try_into().unwrap());
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(sigma1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sigma0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut output = [0_u8; 32];
    for (chunk, value) in output.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    output
}

#[test]
fn local_sha256_matches_published_test_vector() {
    assert_eq!(
        hex_sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
