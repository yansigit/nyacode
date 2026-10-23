//! Harmless generated acceptance corpus, shared by native and RPC probes.
use codex_chatgpt_chat::Capability;
use codex_chatgpt_chat::attachment::AttachmentKind;

pub(super) const SPECIFICATIONS: [(&str, &str, &str, AttachmentKind, Capability); 5] = [
    (
        "fixture.png",
        "PNG-CEDAR-731",
        "0b81b1ad46a19b644fb4048be86577b6e88aaff406ee7bafc561e998b5469600",
        AttachmentKind::Png,
        Capability::PngUpload,
    ),
    (
        "fixture.jpg",
        "JPEG-MAPLE-482",
        "e991e3e7162eda49abc8ba3d8c41dda75eb075795e830c07e57ebbcb34dc1065",
        AttachmentKind::Jpeg,
        Capability::JpegUpload,
    ),
    (
        "fixture.webp",
        "WEBP-ASPEN-956",
        "afcefd8a251bf95a75924734ee7c283bc5571c0763407c444c64a206e86ecffc",
        AttachmentKind::Webp,
        Capability::WebpUpload,
    ),
    (
        "fixture.pdf",
        "PDF-WILLOW-364",
        "93a909cef29ee735cc7d835b07fa1c711df21d9206342805b9b79084024029b1",
        AttachmentKind::Pdf,
        Capability::PdfUpload,
    ),
    (
        "fixture.txt",
        "UTF8-BIRCH-825",
        "d8a37a99a3b426f3da18cce67bb98cc6cc98b6c85d420b1046ea899c83a2d6cc",
        AttachmentKind::Utf8,
        Capability::Utf8Upload,
    ),
];
