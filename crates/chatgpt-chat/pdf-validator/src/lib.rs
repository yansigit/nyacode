//! Import-free PDF validation. The host bounds all memory and execution.
//! This module never returns document contents or parser diagnostics.

use std::collections::HashSet;

const MAX_INPUT_BYTES: u32 = 32 * 1024 * 1024;

enum AttachmentError {
    InvalidFormat,
    Limit,
}

/// Each validation owns a fresh Wasm instance, so its entire memory is freed
/// together. The host can only request this bounded input allocation.
#[unsafe(no_mangle)]
pub extern "C" fn allocate(size: u32) -> *mut u8 {
    if size == 0 || size > MAX_INPUT_BYTES {
        return std::ptr::null_mut();
    }
    Box::into_raw(vec![0u8; size as usize].into_boxed_slice()) as *mut u8
}

/// # Safety
/// The trusted host passes the pointer returned by `allocate` and that exact
/// allocation size after copying the selected bytes into the instance memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn validate(ptr: *const u8, size: u32) -> u32 {
    if ptr.is_null() || size == 0 || size > MAX_INPUT_BYTES {
        return 2;
    }
    // SAFETY: the host supplies its live allocation, as required above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size as usize) };
    match validate_pdf(bytes) {
        Ok(()) => 0,
        Err(AttachmentError::InvalidFormat) => 1,
        Err(AttachmentError::Limit) => 2,
    }
}

fn validate_pdf(bytes: &[u8]) -> Result<(), AttachmentError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(AttachmentError::InvalidFormat);
    }
    let options = lopdf::LoadOptions {
        strict: true,
        max_decompressed_size: Some(8 * 1024 * 1024),
        ..Default::default()
    };
    let document = lopdf::Document::load_mem_with_options(bytes, options)
        .map_err(|_| AttachmentError::InvalidFormat)?;
    if document.objects.len() > 32768 {
        return Err(AttachmentError::Limit);
    }
    let catalog = document
        .catalog()
        .map_err(|_| AttachmentError::InvalidFormat)?;
    if catalog.get(b"Type").and_then(lopdf::Object::as_name).ok() != Some(b"Catalog".as_slice()) {
        return Err(AttachmentError::InvalidFormat);
    }
    let pages = catalog
        .get(b"Pages")
        .and_then(lopdf::Object::as_reference)
        .map_err(|_| AttachmentError::InvalidFormat)?;
    let count = pdf_pages(&document, pages, None, 0, &mut HashSet::new())?;
    if count == 0 {
        return Err(AttachmentError::InvalidFormat);
    }
    Ok(())
}

fn pdf_pages(
    document: &lopdf::Document,
    id: lopdf::ObjectId,
    parent: Option<lopdf::ObjectId>,
    depth: usize,
    visited: &mut HashSet<lopdf::ObjectId>,
) -> Result<usize, AttachmentError> {
    if depth > 64 || visited.len() >= 10000 {
        return Err(AttachmentError::Limit);
    }
    if !visited.insert(id) {
        return Err(AttachmentError::InvalidFormat);
    }
    let dictionary = document
        .get_object(id)
        .and_then(lopdf::Object::as_dict)
        .map_err(|_| AttachmentError::InvalidFormat)?;
    if let Some(parent) = parent
        && dictionary
            .get(b"Parent")
            .and_then(lopdf::Object::as_reference)
            .ok()
            != Some(parent)
    {
        return Err(AttachmentError::InvalidFormat);
    }
    match dictionary
        .get(b"Type")
        .and_then(lopdf::Object::as_name)
        .map_err(|_| AttachmentError::InvalidFormat)?
    {
        b"Page" => Ok(1),
        b"Pages" => {
            let children = dictionary
                .get(b"Kids")
                .and_then(lopdf::Object::as_array)
                .map_err(|_| AttachmentError::InvalidFormat)?;
            let mut count = 0;
            for child in children {
                let child = child
                    .as_reference()
                    .map_err(|_| AttachmentError::InvalidFormat)?;
                count += pdf_pages(document, child, Some(id), depth + 1, visited)?;
                if count > 10000 {
                    return Err(AttachmentError::Limit);
                }
            }
            if dictionary
                .get(b"Count")
                .and_then(lopdf::Object::as_i64)
                .ok()
                != Some(count as i64)
            {
                return Err(AttachmentError::InvalidFormat);
            }
            Ok(count)
        }
        _ => Err(AttachmentError::InvalidFormat),
    }
}
