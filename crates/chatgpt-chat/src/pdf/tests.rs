use super::*;
use pretty_assertions::assert_eq;
use sha2::Digest;
use sha2::Sha256;

const SPIN: &[u8] = br#"(module (func (export "test") (result i32) (loop br 0) (i32.const 0)))"#;

fn run(module: &[u8], budget: &mut Budget<'_>) -> Result<u32, AttachmentError> {
    let module = compile(module)?;
    let (mut store, instance) = instantiate(&module)?;
    budget.refill(&mut store, FUEL_QUANTUM)?;
    let test = instance
        .get_typed_func::<(), u32>(&store, "test")
        .map_err(trap)?;
    let call = test.call_resumable(&mut store, ()).map_err(trap)?;
    budget.finish(call, &mut store)
}

#[test]
fn bundled_module_and_sources_match_the_reviewed_manifest() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../pdf-validator/manifest.json")).unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(VALIDATOR)),
        manifest["artifact_sha256"].as_str().unwrap()
    );
    for (name, bytes) in [
        (
            "Cargo.toml",
            include_bytes!("../../pdf-validator/Cargo.toml").as_slice(),
        ),
        (
            "Cargo.lock",
            include_bytes!("../../pdf-validator/Cargo.lock").as_slice(),
        ),
        (
            "src/lib.rs",
            include_bytes!("../../pdf-validator/src/lib.rs").as_slice(),
        ),
        (
            "build.py",
            include_bytes!("../../pdf-validator/build.py").as_slice(),
        ),
    ] {
        assert_eq!(
            hex::encode(Sha256::digest(bytes)),
            manifest["input_sha256"][name].as_str().unwrap(),
            "validator input changed: {name}"
        );
    }
    assert_eq!(compile(VALIDATOR).unwrap().imports().count(), 0);
}

#[test]
fn imports_and_start_functions_cannot_reach_the_host() {
    for module in [
        br#"(module (import "env" "log" (func)))"#.as_slice(),
        br#"(module (func $start) (start $start))"#.as_slice(),
    ] {
        assert!(matches!(
            compile(module),
            Err(AttachmentError::InvalidFormat)
        ));
    }
}

#[test]
fn allocation_and_stack_limits_interrupt_execution() {
    let cancel = CancellationToken::new();
    for module in [
        br#"(module (memory 1) (func (export "test") (result i32) (memory.grow (i32.const 4096))))"#.as_slice(),
        br#"(module (func $recur (export "test") (result i32) (call $recur)))"#.as_slice(),
        br#"(module (table 1 funcref) (func (export "test") (result i32) (table.grow (ref.null func) (i32.const 2048))))"#.as_slice(),
    ] {
        assert_eq!(run(module, &mut Budget::new(&cancel)), Err(AttachmentError::Limit));
    }
}

#[test]
fn fuel_and_elapsed_limits_stop_infinite_execution() {
    let cancel = CancellationToken::new();
    let mut budget = Budget::new(&cancel);
    budget.remaining = 2 * FUEL_QUANTUM;
    assert_eq!(run(SPIN, &mut budget), Err(AttachmentError::Limit));

    let mut budget = Budget::new(&cancel);
    budget.remaining = u64::MAX;
    budget.deadline = Instant::now() + Duration::from_millis(10);
    assert_eq!(run(SPIN, &mut budget), Err(AttachmentError::Limit));
}

#[test]
fn cancellation_stops_a_suspended_parse_without_resuming_it() {
    let cancel = CancellationToken::new();
    let module = compile(SPIN).unwrap();
    let (mut store, instance) = instantiate(&module).unwrap();
    let mut budget = Budget::new(&cancel);
    budget.refill(&mut store, FUEL_QUANTUM).unwrap();
    let call = instance
        .get_typed_func::<(), u32>(&store, "test")
        .unwrap()
        .call_resumable(&mut store, ())
        .unwrap();
    assert!(matches!(call, TypedResumableCall::OutOfFuel(_)));
    cancel.cancel();
    assert_eq!(
        budget.finish(call, &mut store),
        Err(AttachmentError::Cancelled)
    );
    assert_eq!(
        validate(b"%PDF-1.7", &cancel),
        Err(AttachmentError::Cancelled)
    );
}

// Correct cross-reference offsets let these tests reach the parser rather than
// merely exercising the header check. All contents are generated and harmless.
fn document(extra: &str) -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        extra,
    ];
    let mut pdf = String::from("%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    pdf.into_bytes()
}

#[test]
fn valid_large_input_allocation_can_cross_multiple_fuel_quanta() {
    let content = "x".repeat(4 * 1024 * 1024);
    let bytes = document(&format!(
        "<< /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    ));
    assert_eq!(validate(&bytes, &CancellationToken::new()), Ok(()));
}

#[test]
fn malformed_deeply_nested_and_encrypted_documents_are_rejected() {
    let cancel = CancellationToken::new();
    let nested = format!("{}0{}", "[".repeat(2000), "]".repeat(2000));
    assert!(validate(&document(&nested), &cancel).is_err());
    let mut encrypted = String::from_utf8(document("null")).unwrap();
    encrypted = encrypted.replace("/Size 5", "/Encrypt 4 0 R /Size 5");
    assert!(validate(encrypted.as_bytes(), &cancel).is_err());
    assert!(
        validate(
            &document("<< /Length 100 >>\nstream\nshort\nendstream"),
            &cancel
        )
        .is_err()
    );
    assert_eq!(validate(&document("null"), &cancel), Ok(()));
}

#[test]
fn predictor_row_allocation_is_limited_inside_the_actual_pdf_parser() {
    // Zlib of a single zero byte, encoded as ASCII hex. The tiny stream asks
    // for a 256 MiB PNG row before the parser can inspect decoded objects.
    let compressed = "789c63000000010001>";
    let bytes = document(&format!(
        "<< /Type /ObjStm /N 1 /First 4 /Filter [/ASCIIHexDecode /FlateDecode] /DecodeParms << /Predictor 12 /Columns 268435456 /Colors 1 /BitsPerComponent 8 >> /Length {} >>\nstream\n{compressed}\nendstream",
        compressed.len()
    ));
    assert_eq!(
        validate(&bytes, &CancellationToken::new()),
        Err(AttachmentError::Limit)
    );
    // A fresh instance remains usable after the rejected allocation.
    assert_eq!(
        validate(&document("null"), &CancellationToken::new()),
        Ok(())
    );
}
