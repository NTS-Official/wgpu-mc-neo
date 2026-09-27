//! Checks that the JVM side of the two bridges still matches this crate.
//!
//! There are two ways across, and neither is checked by either compiler:
//!
//!  * **JNI** - `neoforge/src/main/kotlin/dev/birb/wgpu/rust/WgpuNative.kt` declares `external fun`s
//!    that are resolved by name against the `#[jni_fn]` implementations here. A declaration with no
//!    implementation is not a compile error anywhere: it throws `UnsatisfiedLinkError` - an `Error`,
//!    so `catch (e: Exception)` does not see it - the first time that code path runs.
//!  * **C ABI** - `WmNative.kt` binds the exports of this cdylib with the FFM API, by name, and
//!    reads the structs `bindings.h` declares by *byte offset*. A wrong offset is not a compile
//!    error either; it reads whatever field happens to live there.
//!
//! The Kotlin files are pulled in with `include_str!`, so editing one of them re-runs these tests.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};

use crate::blaze::{
    BindGroupEntryDescriptor, BlazeAttachmentDescriptor, BlazeBindGroupLayout, BlazeBlendState,
    BlazeColorTargetState, BlazeDepthStencilState, BlazeRenderPassDescriptor, DrawBinding,
    DrawCall, PlanBinding, RawArray, RenderPipeline, VertexFormat, VertexFormatElement,
};

const WM_NATIVE_KT: &str =
    include_str!("../../../neoforge/src/main/kotlin/dev/birb/wgpu/rust/WmNative.kt");
const WGPU_NATIVE_KT: &str =
    include_str!("../../../neoforge/src/main/kotlin/dev/birb/wgpu/rust/WgpuNative.kt");

/// Every file that exports something across one of the two bridges.
const BRIDGE_SOURCES: &[&str] = &[
    include_str!("blaze.rs"),
    include_str!("device.rs"),
    include_str!("entity.rs"),
    include_str!("lib.rs"),
    include_str!("palette.rs"),
    include_str!("pia.rs"),
    include_str!("renderer.rs"),
];

// ---------------------------------------------------------------------------------------------
// Text helpers. No regex crate: the shapes being parsed are fixed and few.
// ---------------------------------------------------------------------------------------------

/// The body of the bracket group that starts at `open`, and the index of its closer.
///
/// `(`, `<` and `[` all nest, which is what makes this work on `Box<RawArray<T>>` and on
/// `MemoryLayout.paddingLayout(4)` alike.
fn group(source: &str, open: usize) -> (&str, usize) {
    let bytes = source.as_bytes();
    let mut depth = 0usize;

    for i in open..bytes.len() {
        match bytes[i] {
            b'(' | b'<' | b'[' => depth += 1,
            b')' | b'>' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return (&source[open + 1..i], i);
                }
            }
            _ => {}
        }
    }

    panic!(
        "unbalanced brackets in {:?}",
        &source[open..(open + 40).min(source.len())]
    );
}

/// Splits an argument or field list at its top-level commas.
fn split_top_level(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();

    for c in body.chars() {
        match c {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }

    parts
}

/// The parameter list of the `fn` called `name`, and the number of parameters in it.
///
/// Handles the `<'local>` a JNI entry point may carry between the name and the parenthesis.
fn fn_parameters(source: &str, name: &str) -> usize {
    let needle = format!("fn {name}");
    let mut from = 0usize;

    loop {
        let start = source[from..]
            .find(&needle)
            .unwrap_or_else(|| panic!("no fn {name} in the sources"))
            + from;

        // `fn foo` must not match `fn foobar`.
        let after = start + needle.len();
        let next = source[after..].chars().next().unwrap_or(' ');
        if !(next == '(' || next == '<' || next.is_whitespace()) {
            from = after;
            continue;
        }

        let mut open = after;
        while source.as_bytes()[open].is_ascii_whitespace() {
            open += 1;
        }
        if source.as_bytes()[open] == b'<' {
            open = group(source, open).1 + 1;
            while source.as_bytes()[open].is_ascii_whitespace() {
                open += 1;
            }
        }

        assert_eq!(
            source.as_bytes()[open],
            b'(',
            "{name}: expected a parameter list"
        );
        return split_top_level(group(source, open).0).len();
    }
}

/// Every `const val NAME = 12L` in a Kotlin file.
fn kotlin_constants(source: &str) -> HashMap<String, i64> {
    let mut out = HashMap::new();

    for line in source.lines() {
        let Some(rest) = line.trim().strip_prefix("const val ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        if let Ok(value) = value.trim().trim_end_matches('L').parse::<i64>() {
            out.insert(name.trim().to_string(), value);
        }
    }

    out
}

/// The fields of a `val NAME: MemoryLayout = MemoryLayout.structLayout(...)` declaration.
///
/// The declaration may be wrapped onto two lines, which is why the layout expression is looked up
/// separately from the name.
fn kotlin_layout(source: &str, name: &str) -> Vec<String> {
    let declaration = format!("val {name}: MemoryLayout");
    let start = source
        .find(&declaration)
        .unwrap_or_else(|| panic!("WmNative.kt has no layout {name}"));
    let body_at = source[start..]
        .find("MemoryLayout.structLayout(")
        .unwrap_or_else(|| panic!("{name} is not a structLayout"))
        + start
        + "MemoryLayout.structLayout".len();

    let (body, _) = group(source, body_at);
    // Fields are annotated with the Rust field they stand for, and a comment runs to the end of
    // its line: drop them before splitting, or the next field's name gets glued to the comment.
    let body: String = body
        .lines()
        .map(|line| match line.find("//") {
            Some(comment) => &line[..comment],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");

    split_top_level(&body)
        .into_iter()
        .map(|field| field.replace(' ', ""))
        .collect()
}

/// The size and alignment the FFM API gives one entry of a Kotlin struct layout.
fn layout_field(field: &str) -> (u64, u64) {
    match field {
        "PTR" | "ADDRESS" => (8, 8),
        "LONG" => (8, 8),
        "INT" => (4, 4),
        "FLOAT" => (4, 4),
        "DOUBLE" => (8, 8),
        other => {
            let padding = other
                .strip_prefix("MemoryLayout.paddingLayout(")
                .and_then(|rest| rest.strip_suffix(')'))
                .and_then(|size| size.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("unknown layout entry {other}"));

            (padding, 1)
        }
    }
}

/// Where each field of a `#[repr(C)]` struct lands, and how big the struct is.
fn layout_offsets(fields: &[String]) -> (Vec<u64>, u64) {
    let mut offsets = Vec::with_capacity(fields.len());
    let mut offset = 0u64;
    let mut alignment = 1u64;

    for field in fields {
        let (size, field_alignment) = layout_field(field);
        alignment = alignment.max(field_alignment);
        offset = offset.div_ceil(field_alignment) * field_alignment;
        offsets.push(offset);
        offset += size;
    }

    (offsets, offset.div_ceil(alignment) * alignment)
}

/// The variants of `pub enum Name { ... }` in a Rust source, with their discriminants.
///
/// The body is read from the source rather than listed here so that adding a variant to the enum
/// cannot quietly skip this check.
fn rust_enum_variants(source: &str, name: &str) -> Vec<(String, u64)> {
    let needle = format!("pub enum {name} {{");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("no enum {name}"))
        + needle.len();
    let end = source[start..].find('}').expect("unterminated enum") + start;

    let mut variants = Vec::new();
    let mut next = 0u64;

    for entry in source[start..end].split(',') {
        // Doc comments on variants carry commas of their own.
        let entry: String = entry
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join(" ");
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        let (variant, value) = match entry.split_once('=') {
            Some((variant, value)) => (
                variant.trim(),
                value.trim().parse::<u64>().expect("enum discriminant"),
            ),
            None => (entry, next),
        };

        next = value + 1;
        variants.push((variant.to_string(), value));
    }

    variants
}

/// `SrcAlphaSaturate` -> `SRC_ALPHA_SATURATE`, which is how the Kotlin constants are spelled.
///
/// A digit does not start a new word: `RGB10A2_UNORM` is one word up to the underscore, and
/// `RGB10_A2_UNORM` is not a constant WmNative.kt has.
fn screaming_snake(name: &str) -> String {
    let mut out = String::new();
    let mut previous = '\0';

    for c in name.chars() {
        if c.is_ascii_uppercase() && previous.is_ascii_lowercase() {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
        previous = c;
    }

    out
}

// ---------------------------------------------------------------------------------------------
// The bridges.
// ---------------------------------------------------------------------------------------------

/// Every `external fun name(args)` in `WgpuNative.kt`, with its JVM argument count.
fn kotlin_jni_declarations(source: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut from = 0usize;

    while let Some(found) = source[from..].find("external fun ") {
        let start = found + from + "external fun ".len();
        let name: String = source[start..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let open = start + name.len();
        assert_eq!(
            source.as_bytes()[open],
            b'(',
            "{name}: expected a parameter list"
        );

        out.push((name, split_top_level(group(source, open).0).len()));
        from = open;
    }

    out
}

/// Every `#[jni_fn("...")]` implementation in this crate, as `class::method` and argument count.
///
/// The first two parameters of a JNI entry point are the `JNIEnv` and the class, so the Java
/// arguments are the rest.
fn rust_jni_implementations(sources: &[&str]) -> Vec<(String, usize)> {
    let mut out = Vec::new();

    for source in sources {
        let mut from = 0usize;

        while let Some(found) = source[from..].find("#[jni_fn(\"") {
            let start = found + from + "#[jni_fn(\"".len();
            let end = start
                + source[start..]
                    .find('"')
                    .expect("unterminated jni_fn target");
            let target = &source[start..end];

            // `jni_fn` names the method after the Rust function unless the target says otherwise.
            let (class, method) = match target.split_once("::") {
                Some((class, method)) => (class.to_string(), Some(method.to_string())),
                None => (target.to_string(), None),
            };

            let after = end + 2;
            let fn_at = source[after..]
                .find("fn ")
                .expect("jni_fn without a function")
                + after
                + 3;
            let name: String = source[fn_at..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let method = method.unwrap_or_else(|| name.clone());

            out.push((
                format!("{class}::{method}"),
                fn_parameters(source, &name) - 2,
            ));
            from = fn_at;
        }
    }

    out
}

/// Every `handle("name", FunctionDescriptor.of...(...))` in `WmNative.kt`, with its argument count.
fn kotlin_c_abi_bindings(source: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut from = 0usize;

    while let Some(found) = source[from..].find("handle(") {
        let start = found + from + "handle(".len();

        // `handle` is also the name of the helper that resolves a symbol; only a call whose first
        // argument is a string literal is a binding.
        let mut quote = start;
        while source.as_bytes()[quote].is_ascii_whitespace() {
            quote += 1;
        }
        if source.as_bytes()[quote] != b'"' {
            from = start;
            continue;
        }
        let after_quote = quote + 1;
        let end_quote = after_quote + source[after_quote..].find('"').expect("unterminated name");
        let name = source[after_quote..end_quote].to_string();

        let descriptor = source[end_quote..]
            .find("FunctionDescriptor.")
            .expect("handle without a descriptor")
            + end_quote
            + "FunctionDescriptor.".len();
        let returns_nothing = source[descriptor..].starts_with("ofVoid(");
        let open = source[descriptor..]
            .find('(')
            .expect("descriptor without arguments")
            + descriptor;
        let args = split_top_level(group(source, open).0).len();

        out.push((name, if returns_nothing { args } else { args - 1 }));
        from = open;
    }

    out
}

/// Every `#[no_mangle] pub extern "C" fn` in this crate, with its parameter count.
fn rust_c_abi_exports(sources: &[&str]) -> Vec<(String, usize)> {
    let mut out = Vec::new();

    for source in sources {
        let mut from = 0usize;

        while let Some(found) = source[from..].find("no_mangle)]") {
            let after = found + from + "no_mangle)]".len();
            let Some(fn_at) = source[after..].find("fn ") else {
                break;
            };
            let fn_at = fn_at + after + 3;
            let name: String = source[fn_at..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();

            out.push((name.clone(), fn_parameters(source, &name)));
            from = fn_at;
        }
    }

    out
}

// ---------------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn every_struct_the_jvm_reads_by_offset_still_has_that_layout() {
    let constants = kotlin_constants(WM_NATIVE_KT);

    check_layout(
        "ATTACHMENT_F32X4",
        size_of::<BlazeAttachmentDescriptor<'static, [f32; 4]>>(),
        &[
            (
                "texture_view",
                offset_of!(BlazeAttachmentDescriptor<'static, [f32; 4]>, texture_view),
            ),
            (
                "clear_value",
                offset_of!(BlazeAttachmentDescriptor<'static, [f32; 4]>, clear_value),
            ),
        ],
    );
    check_layout(
        "ATTACHMENT_F64",
        size_of::<BlazeAttachmentDescriptor<'static, f64>>(),
        &[
            (
                "texture_view",
                offset_of!(BlazeAttachmentDescriptor<'static, f64>, texture_view),
            ),
            (
                "clear_value",
                offset_of!(BlazeAttachmentDescriptor<'static, f64>, clear_value),
            ),
        ],
    );
    check_layout(
        "RENDER_PASS_DESCRIPTOR",
        size_of::<BlazeRenderPassDescriptor<'static>>(),
        &[
            (
                "attachments",
                offset_of!(BlazeRenderPassDescriptor<'static>, attachments),
            ),
            (
                "depth_attachment",
                offset_of!(BlazeRenderPassDescriptor<'static>, depth_attachment),
            ),
        ],
    );
    check_layout(
        "BIND_GROUP_ENTRY",
        size_of::<BindGroupEntryDescriptor>(),
        &[
            ("type_", offset_of!(BindGroupEntryDescriptor, type_)),
            ("name", offset_of!(BindGroupEntryDescriptor, name)),
            (
                "texture_format",
                offset_of!(BindGroupEntryDescriptor, texture_format),
            ),
        ],
    );
    check_layout(
        "BIND_GROUP_LAYOUT",
        size_of::<BlazeBindGroupLayout>(),
        &[("entries", offset_of!(BlazeBindGroupLayout, entries))],
    );
    check_layout(
        "COLOR_TARGET_STATE",
        size_of::<BlazeColorTargetState>(),
        &[
            ("blend", offset_of!(BlazeColorTargetState, blend)),
            ("format", offset_of!(BlazeColorTargetState, format)),
            ("write_mask", offset_of!(BlazeColorTargetState, write_mask)),
        ],
    );
    check_layout(
        "BLEND_STATE",
        size_of::<BlazeBlendState>(),
        &[
            ("src_color", offset_of!(BlazeBlendState, src_color)),
            ("dst_color", offset_of!(BlazeBlendState, dst_color)),
            ("src_alpha", offset_of!(BlazeBlendState, src_alpha)),
            ("dst_alpha", offset_of!(BlazeBlendState, dst_alpha)),
        ],
    );
    check_layout(
        "DEPTH_STENCIL_STATE",
        size_of::<BlazeDepthStencilState>(),
        &[
            (
                "compare_function",
                offset_of!(BlazeDepthStencilState, compare_function),
            ),
            ("active", offset_of!(BlazeDepthStencilState, active)),
            (
                "bias_constant",
                offset_of!(BlazeDepthStencilState, bias_constant),
            ),
            (
                "bias_slope_scale",
                offset_of!(BlazeDepthStencilState, bias_slope_scale),
            ),
        ],
    );
    check_layout(
        "VERTEX_FORMAT_ELEMENT",
        size_of::<VertexFormatElement>(),
        &[
            ("offset", offset_of!(VertexFormatElement, offset)),
            ("format", offset_of!(VertexFormatElement, format)),
            ("name", offset_of!(VertexFormatElement, name)),
        ],
    );
    check_layout(
        "VERTEX_FORMAT",
        size_of::<VertexFormat>(),
        &[
            ("elements", offset_of!(VertexFormat, elements)),
            ("vertex_size", offset_of!(VertexFormat, vertex_size)),
        ],
    );
    check_layout(
        "RENDER_PIPELINE",
        size_of::<RenderPipeline>(),
        &[
            ("name", offset_of!(RenderPipeline, name)),
            (
                "bind_group_layouts",
                offset_of!(RenderPipeline, bind_group_layouts),
            ),
            (
                "color_target_states",
                offset_of!(RenderPipeline, color_target_states),
            ),
            (
                "depth_stencil_state",
                offset_of!(RenderPipeline, depth_stencil_state),
            ),
            ("vertex_formats", offset_of!(RenderPipeline, vertex_formats)),
            ("vertex_shader", offset_of!(RenderPipeline, vertex_shader)),
            (
                "fragment_shader",
                offset_of!(RenderPipeline, fragment_shader),
            ),
            ("directives", offset_of!(RenderPipeline, directives)),
            ("frag_state", offset_of!(RenderPipeline, frag_state)),
            (
                "primitive_topology",
                offset_of!(RenderPipeline, primitive_topology),
            ),
            ("cull", offset_of!(RenderPipeline, cull)),
        ],
    );

    // `RawArray`'s fields are private, so only its size is readable from here. The JVM writes
    // `{ contents, size }` through `writeRawArray`, which is why the size has to be 16.
    assert_eq!(
        kotlin_layout(WM_NATIVE_KT, "RAW_ARRAY").len(),
        2,
        "RawArray is a two-field struct"
    );
    assert_eq!(
        size_of::<RawArray<u8>>(),
        layout_offsets(&kotlin_layout(WM_NATIVE_KT, "RAW_ARRAY")).1 as usize,
        "RawArray<u8>"
    );

    // The offsets the JVM writes through, named one by one in WmNative.kt.
    check_offset(
        &constants,
        "FIELD_TEXTURE_VIEW",
        offset_of!(BlazeAttachmentDescriptor<'static, [f32; 4]>, texture_view),
    );
    check_offset(
        &constants,
        "FIELD_CLEAR_VALUE",
        offset_of!(BlazeAttachmentDescriptor<'static, [f32; 4]>, clear_value),
    );
    check_offset(
        &constants,
        "BIND_GROUP_ENTRY_TYPE",
        offset_of!(BindGroupEntryDescriptor, type_),
    );
    check_offset(
        &constants,
        "BIND_GROUP_ENTRY_NAME",
        offset_of!(BindGroupEntryDescriptor, name),
    );
    check_offset(
        &constants,
        "BIND_GROUP_ENTRY_FORMAT",
        offset_of!(BindGroupEntryDescriptor, texture_format),
    );
    check_offset(
        &constants,
        "COLOR_TARGET_BLEND",
        offset_of!(BlazeColorTargetState, blend),
    );
    check_offset(
        &constants,
        "COLOR_TARGET_FORMAT",
        offset_of!(BlazeColorTargetState, format),
    );
    check_offset(
        &constants,
        "COLOR_TARGET_WRITE_MASK",
        offset_of!(BlazeColorTargetState, write_mask),
    );
    check_offset(
        &constants,
        "BLEND_STATE_SRC_COLOR",
        offset_of!(BlazeBlendState, src_color),
    );
    check_offset(
        &constants,
        "BLEND_STATE_DST_COLOR",
        offset_of!(BlazeBlendState, dst_color),
    );
    check_offset(
        &constants,
        "BLEND_STATE_SRC_ALPHA",
        offset_of!(BlazeBlendState, src_alpha),
    );
    check_offset(
        &constants,
        "BLEND_STATE_DST_ALPHA",
        offset_of!(BlazeBlendState, dst_alpha),
    );
    check_offset(
        &constants,
        "DEPTH_STENCIL_COMPARE",
        offset_of!(BlazeDepthStencilState, compare_function),
    );
    check_offset(
        &constants,
        "DEPTH_STENCIL_ACTIVE",
        offset_of!(BlazeDepthStencilState, active),
    );
    check_offset(
        &constants,
        "DEPTH_STENCIL_BIAS_CONSTANT",
        offset_of!(BlazeDepthStencilState, bias_constant),
    );
    check_offset(
        &constants,
        "DEPTH_STENCIL_BIAS_SLOPE_SCALE",
        offset_of!(BlazeDepthStencilState, bias_slope_scale),
    );
    check_offset(
        &constants,
        "VERTEX_FORMAT_ELEMENT_OFFSET",
        offset_of!(VertexFormatElement, offset),
    );
    check_offset(
        &constants,
        "VERTEX_FORMAT_ELEMENT_FORMAT",
        offset_of!(VertexFormatElement, format),
    );
    check_offset(
        &constants,
        "VERTEX_FORMAT_ELEMENT_NAME",
        offset_of!(VertexFormatElement, name),
    );
    check_offset(
        &constants,
        "VERTEX_FORMAT_ELEMENTS",
        offset_of!(VertexFormat, elements),
    );
    check_offset(
        &constants,
        "VERTEX_FORMAT_VERTEX_SIZE",
        offset_of!(VertexFormat, vertex_size),
    );
    check_offset(
        &constants,
        "PIPELINE_NAME",
        offset_of!(RenderPipeline, name),
    );
    check_offset(
        &constants,
        "PIPELINE_BIND_GROUP_LAYOUTS",
        offset_of!(RenderPipeline, bind_group_layouts),
    );
    check_offset(
        &constants,
        "PIPELINE_COLOR_TARGETS",
        offset_of!(RenderPipeline, color_target_states),
    );
    check_offset(
        &constants,
        "PIPELINE_DEPTH_STENCIL",
        offset_of!(RenderPipeline, depth_stencil_state),
    );
    check_offset(
        &constants,
        "PIPELINE_VERTEX_FORMATS",
        offset_of!(RenderPipeline, vertex_formats),
    );
    check_offset(
        &constants,
        "PIPELINE_VERTEX_SHADER",
        offset_of!(RenderPipeline, vertex_shader),
    );
    check_offset(
        &constants,
        "PIPELINE_FRAGMENT_SHADER",
        offset_of!(RenderPipeline, fragment_shader),
    );
    check_offset(
        &constants,
        "PIPELINE_DIRECTIVES",
        offset_of!(RenderPipeline, directives),
    );
    check_offset(
        &constants,
        "PIPELINE_FRAG_STATE",
        offset_of!(RenderPipeline, frag_state),
    );
    check_offset(
        &constants,
        "PIPELINE_TOPOLOGY",
        offset_of!(RenderPipeline, primitive_topology),
    );
    check_offset(
        &constants,
        "PIPELINE_CULL",
        offset_of!(RenderPipeline, cull),
    );

    // The draw call and the two tables inside it. The JVM writes a draw by these offsets, and the
    // fields a draw is numbered by - the combination and whether the table came with it - are the
    // last two, which is exactly the kind of thing a struct edit moves without saying so.
    check_offset(
        &constants,
        "DRAW_CALL_VERTEX_BUFFERS",
        offset_of!(DrawCall, vertex_buffers),
    );
    check_offset(
        &constants,
        "DRAW_CALL_BINDINGS",
        offset_of!(DrawCall, bindings),
    );
    check_offset(&constants, "DRAW_CALL_COMBO", offset_of!(DrawCall, combo));
    check_offset(
        &constants,
        "DRAW_CALL_BINDINGS_PRESENT",
        offset_of!(DrawCall, bindings_present),
    );
    check_offset(
        &constants,
        "DRAW_BINDING_KIND",
        offset_of!(DrawBinding, kind),
    );
    check_offset(
        &constants,
        "DRAW_BINDING_RESOURCE",
        offset_of!(DrawBinding, resource),
    );
    check_offset(
        &constants,
        "DRAW_BINDING_OFFSET",
        offset_of!(DrawBinding, offset),
    );
    check_offset(
        &constants,
        "DRAW_BINDING_LENGTH",
        offset_of!(DrawBinding, length),
    );
    check_offset(
        &constants,
        "PLAN_BINDING_NAME",
        offset_of!(PlanBinding, name),
    );
    check_offset(
        &constants,
        "PLAN_BINDING_DECLARED_NAME",
        offset_of!(PlanBinding, declared_name),
    );
    check_offset(&constants, "PLAN_BINDING_SET", offset_of!(PlanBinding, set));
    check_offset(
        &constants,
        "PLAN_BINDING_BINDING",
        offset_of!(PlanBinding, binding),
    );
    check_offset(
        &constants,
        "PLAN_BINDING_KIND",
        offset_of!(PlanBinding, kind),
    );
    check_offset(
        &constants,
        "PLAN_BINDING_DYNAMIC",
        offset_of!(PlanBinding, dynamic),
    );

    fn check_layout(layout: &str, rust_size: usize, fields: &[(&str, usize)]) {
        // Trailing padding is a layout entry without a field behind it, which is how the 24-byte
        // `BlazeColorTargetState` is spelled out.
        let entries: Vec<(String, u64)> = kotlin_layout(WM_NATIVE_KT, layout)
            .into_iter()
            .scan(0u64, |offset, entry| {
                let (size, alignment) = layout_field(&entry);
                *offset = offset.div_ceil(alignment) * alignment;
                let at = *offset;
                *offset += size;
                Some((entry, at))
            })
            .filter(|(entry, _)| !entry.starts_with("MemoryLayout.paddingLayout("))
            .collect();

        assert_eq!(entries.len(), fields.len(), "{layout}: field count");
        for (index, (name, rust_offset)) in fields.iter().enumerate() {
            assert_eq!(
                entries[index].1 as usize, *rust_offset,
                "{layout}.{name}: WmNative.kt writes it at {}, Rust has it at {rust_offset}",
                entries[index].1
            );
        }

        let size = layout_offsets(&kotlin_layout(WM_NATIVE_KT, layout)).1;
        assert_eq!(
            size as usize, rust_size,
            "{layout}: WmNative.kt says {size} bytes, Rust says {rust_size}"
        );
    }

    fn check_offset(constants: &HashMap<String, i64>, name: &str, rust_offset: usize) {
        let value = *constants
            .get(name)
            .unwrap_or_else(|| panic!("WmNative.kt has no constant {name}"));

        assert_eq!(
            value as usize, rust_offset,
            "{name}: WmNative.kt says {value}, Rust has it at {rust_offset}"
        );
    }
}

#[test]
fn the_enum_numbers_the_jvm_passes_are_the_ones_this_crate_matches_on() {
    let constants = kotlin_constants(WM_NATIVE_KT);

    check_enum(&constants, "GpuFormat", "");
    check_enum(&constants, "UniformType", "ENTRY_");
    check_enum(&constants, "BlendFactor", "BLEND_");
    check_enum(&constants, "CompareFunction", "COMPARE_");
    check_enum(&constants, "PrimitiveTopology", "TOPOLOGY_");

    fn check_enum(constants: &HashMap<String, i64>, name: &str, prefix: &str) {
        let variants = rust_enum_variants(include_str!("blaze.rs"), name);
        assert!(!variants.is_empty(), "{name} has no variants");

        for (variant, value) in variants {
            // Two variants are named for what they are rather than for what the JVM calls them.
            let suffix = match (name, variant.as_str()) {
                ("UniformType", "UBO") => "UNIFORM_BUFFER".to_string(),
                ("PrimitiveTopology", "Tris") => "TRIANGLES".to_string(),
                _ => screaming_snake(&variant),
            };
            let constant = format!("{prefix}{suffix}");
            let kotlin = *constants.get(&constant).unwrap_or_else(|| {
                panic!("WmNative.kt has no constant {constant} for {name}::{variant}")
            });

            assert_eq!(
                kotlin as u64, value,
                "{name}::{variant} is {value} in Rust but {constant} is {kotlin} in Kotlin"
            );
        }
    }
}

#[test]
fn every_jni_declaration_has_an_implementation() {
    let declarations = kotlin_jni_declarations(WGPU_NATIVE_KT);
    let implementations: HashMap<String, usize> = rust_jni_implementations(BRIDGE_SOURCES)
        .into_iter()
        .collect();

    assert!(
        !declarations.is_empty(),
        "no external fun was found in WgpuNative.kt; has the file moved?"
    );

    for (name, arguments) in &declarations {
        let key = format!("dev.birb.wgpu.rust.WgpuNative::{name}");
        let rust_arguments = implementations.get(&key).unwrap_or_else(|| {
            panic!(
                "WgpuNative.{name} is declared but no #[jni_fn] implements it, so calling it \
                 throws UnsatisfiedLinkError"
            )
        });

        assert_eq!(
            rust_arguments, arguments,
            "WgpuNative.{name}: the JVM declares {arguments} arguments, Rust takes {rust_arguments}"
        );
    }

    // And the other way round, which is the direction that fails *silently*: a `#[jni_fn]` nothing
    // declares is a function nobody calls, and a feature that looks implemented on the Rust side and
    // does nothing at all in the game. This is how the fog block was wired: the Rust half went in with
    // the shader, and the declaration was a step the compiler could not ask for.
    for key in implementations.keys() {
        let name = key
            .strip_prefix("dev.birb.wgpu.rust.WgpuNative::")
            .unwrap_or_else(|| panic!("{key} is not a WgpuNative implementation"));

        assert!(
            declarations.iter().any(|(declared, _)| declared == name),
            "{key} is implemented in Rust but nothing in WgpuNative.kt declares it, so it is dead \
             code on that side"
        );
    }
}

#[test]
fn every_c_abi_binding_has_an_export() {
    let bindings = kotlin_c_abi_bindings(WM_NATIVE_KT);
    let exports: HashMap<String, usize> = rust_c_abi_exports(BRIDGE_SOURCES).into_iter().collect();

    assert!(
        !bindings.is_empty(),
        "no handle(...) binding was found in WmNative.kt; has the file moved?"
    );

    for (name, arguments) in &bindings {
        let rust_arguments = exports.get(name).unwrap_or_else(|| {
            panic!("WmNative.kt binds {name}, which this crate does not export")
        });

        assert_eq!(
            rust_arguments, arguments,
            "{name}: WmNative.kt passes {arguments} arguments, Rust takes {rust_arguments}"
        );
    }

    // And the other way round: an export nobody binds is either a leftover or a forgotten binding.
    let bound: Vec<&String> = bindings.iter().map(|(name, _)| name).collect();
    for name in exports.keys() {
        assert!(
            bound.contains(&name),
            "{name} is exported by this crate but nothing in WmNative.kt binds it"
        );
    }
}
