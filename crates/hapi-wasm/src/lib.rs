use std::cell::RefCell;
use std::ptr;
use std::slice;

use hapi_core::{
    compute_unified_diff, fold_message_window, render_markdown_html, StreamingMarkdownEngine,
    WindowMessage,
};

thread_local! {
    static RESULT_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(16 * 1024));
    static STREAM_ENGINE: RefCell<StreamingMarkdownEngine> = RefCell::new(StreamingMarkdownEngine::new());
}

fn store_result(bytes: &[u8]) -> usize {
    RESULT_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        buf.extend_from_slice(bytes);
        buf.len()
    })
}

/// Allocates a UTF-8 input buffer inside linear WASM memory.
#[no_mangle]
pub extern "C" fn hapi_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return ptr::NonNull::<u8>::dangling().as_ptr();
    }
    let mut vec = Vec::<u8>::with_capacity(len);
    let ptr = vec.as_mut_ptr();
    std::mem::forget(vec);
    ptr
}

/// Frees a buffer previously created by [`hapi_alloc`].
///
/// # Safety
/// `ptr` and `len` must match the preceding `hapi_alloc(len)` allocation.
#[no_mangle]
pub unsafe extern "C" fn hapi_dealloc(ptr: *mut u8, len: usize) {
    if len == 0 || ptr.is_null() {
        return;
    }
    let _ = Vec::from_raw_parts(ptr, 0, len);
}

/// Returns a pointer to the shared output buffer populated by the most recent call.
#[no_mangle]
pub extern "C" fn hapi_result_ptr() -> *const u8 {
    RESULT_BUF.with(|cell| cell.borrow().as_ptr())
}

unsafe fn read_utf8<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if len == 0 {
        return Some("");
    }
    if ptr.is_null() {
        return None;
    }
    let bytes = slice::from_raw_parts(ptr, len);
    std::str::from_utf8(bytes).ok()
}

/// Renders standalone Markdown to HTML into the result buffer, returning byte length.
#[no_mangle]
pub unsafe extern "C" fn hapi_render_markdown(ptr: *const u8, len: usize) -> usize {
    let Some(md) = read_utf8(ptr, len) else {
        return 0;
    };
    let html = render_markdown_html(md);
    store_result(html.as_bytes())
}

/// Resets the incremental streaming Markdown block cache.
#[no_mangle]
pub extern "C" fn hapi_reset_stream_markdown() {
    STREAM_ENGINE.with(|cell| {
        *cell.borrow_mut() = StreamingMarkdownEngine::new();
    });
}

/// Incrementally renders cumulative streaming Markdown and writes JSON result to output buffer.
#[no_mangle]
pub unsafe extern "C" fn hapi_render_stream_markdown(ptr: *const u8, len: usize) -> usize {
    let Some(md) = read_utf8(ptr, len) else {
        return 0;
    };
    let rendered = STREAM_ENGINE.with(|cell| cell.borrow_mut().render_cumulative(md));
    match serde_json::to_vec(&rendered) {
        Ok(json) => store_result(&json),
        Err(_) => 0,
    }
}

/// Computes unified diff JSON into the result buffer.
#[no_mangle]
pub unsafe extern "C" fn hapi_compute_diff(
    old_ptr: *const u8,
    old_len: usize,
    new_ptr: *const u8,
    new_len: usize,
    context_lines: usize,
) -> usize {
    let (Some(old), Some(new)) = (read_utf8(old_ptr, old_len), read_utf8(new_ptr, new_len)) else {
        return 0;
    };
    let diff = compute_unified_diff(old, new, context_lines);
    match serde_json::to_vec(&diff) {
        Ok(json) => store_result(&json),
        Err(_) => 0,
    }
}

/// Folds incoming messages into an existing message window and writes serialized JSON.
#[no_mangle]
pub unsafe extern "C" fn hapi_fold_window(
    existing_ptr: *const u8,
    existing_len: usize,
    incoming_ptr: *const u8,
    incoming_len: usize,
    max_window: usize,
) -> usize {
    let (Some(existing_json), Some(incoming_json)) = (
        read_utf8(existing_ptr, existing_len),
        read_utf8(incoming_ptr, incoming_len),
    ) else {
        return 0;
    };
    let Ok(existing) = serde_json::from_str::<Vec<WindowMessage>>(existing_json) else {
        return 0;
    };
    let Ok(incoming) = serde_json::from_str::<Vec<WindowMessage>>(incoming_json) else {
        return 0;
    };
    let folded = fold_message_window(&existing, &incoming, max_window);
    match serde_json::to_vec(&folded) {
        Ok(json) => store_result(&json),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_abi_roundtrip_works_on_host() {
        let md = b"## Heading\n\nBody text";
        let len = unsafe { hapi_render_markdown(md.as_ptr(), md.len()) };
        let html = unsafe {
            let bytes = slice::from_raw_parts(hapi_result_ptr(), len);
            std::str::from_utf8(bytes).unwrap().to_string()
        };
        assert!(html.contains("<h2>Heading</h2>"));
        assert!(html.contains("<p>Body text</p>"));
    }
}
