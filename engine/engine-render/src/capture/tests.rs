use super::{
    CaptureLifecycle, CapturePixelOrder, CaptureRecord, CaptureState, CaptureTicket, ViewportId,
    cancel_capture_lifecycle, capture_buffer_layout, capture_lifecycle, capture_pixel_order,
    capture_rgba8, fail_captures, make_opaque, next_capture_ticket, queued_window_tickets,
    window_capture_support,
};
use ash::vk;

#[test]
fn bgra_readback_is_swizzled_and_row_padding_is_removed() {
    let source = [
        10, 20, 30, 40, 0, 0, 0, 0, // row 0 + padding
        50, 60, 70, 80, 0, 0, 0, 0, // row 1 + padding
    ];
    let rgba = capture_rgba8(&source, 1, 2, 8, CapturePixelOrder::Bgra).unwrap();
    assert_eq!(rgba, [30, 20, 10, 40, 70, 60, 50, 80]);
    assert_eq!(
        capture_pixel_order(vk::Format::B8G8R8A8_SRGB),
        CapturePixelOrder::Bgra
    );
}

#[test]
fn window_capture_needs_transfer_source_and_an_eight_bit_swapchain() {
    assert_eq!(
        window_capture_support(true, vk::Format::B8G8R8A8_UNORM),
        Ok(CapturePixelOrder::Bgra)
    );
    assert_eq!(
        window_capture_support(true, vk::Format::R8G8B8A8_SRGB),
        Ok(CapturePixelOrder::Rgba)
    );
    assert!(window_capture_support(false, vk::Format::B8G8R8A8_SRGB).is_err());
    assert!(window_capture_support(true, vk::Format::A2B10G10R10_UNORM_PACK32).is_err());
}

#[test]
fn unsupported_window_captures_fail_without_touching_viewport_captures() {
    let mut captures = vec![
        CaptureRecord {
            ticket: CaptureTicket(1),
            viewport: Some(ViewportId(0)),
            state: CaptureState::Queued,
        },
        CaptureRecord {
            ticket: CaptureTicket(2),
            viewport: None,
            state: CaptureState::Queued,
        },
        CaptureRecord {
            ticket: CaptureTicket(3),
            viewport: None,
            state: CaptureState::Cancelled,
        },
    ];
    let tickets = queued_window_tickets(&captures);
    assert_eq!(tickets, [CaptureTicket(2)]);
    fail_captures(&mut captures, &tickets, "unsupported");
    assert_eq!(
        capture_lifecycle(&captures[0].state),
        CaptureLifecycle::Queued
    );
    assert_eq!(
        capture_lifecycle(&captures[1].state),
        CaptureLifecycle::Failed
    );
    assert_eq!(
        capture_lifecycle(&captures[2].state),
        CaptureLifecycle::Cancelled
    );
}

#[test]
fn window_capture_alpha_is_made_opaque() {
    let mut rgba = [1, 2, 3, 0, 4, 5, 6, 128];
    make_opaque(&mut rgba);
    assert_eq!(rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
}

#[test]
fn capture_tickets_are_issued_in_sequence() {
    let mut next = 0;
    assert_eq!(next_capture_ticket(&mut next), CaptureTicket(0));
    assert_eq!(next_capture_ticket(&mut next), CaptureTicket(1));
}

#[test]
fn cancellation_is_safe_in_every_lifecycle_state() {
    let cases = [
        (CaptureLifecycle::Queued, CaptureLifecycle::Cancelled),
        (CaptureLifecycle::Submitted, CaptureLifecycle::Submitted),
        (CaptureLifecycle::Completed, CaptureLifecycle::Cancelled),
        (CaptureLifecycle::Failed, CaptureLifecycle::Cancelled),
        (CaptureLifecycle::Cancelled, CaptureLifecycle::Cancelled),
    ];
    for (before, after) in cases {
        assert_eq!(cancel_capture_lifecycle(before), after);
    }
}

#[test]
fn capture_buffer_size_and_row_alignment_are_checked() {
    let tight = capture_buffer_layout(3, 2, 1).unwrap();
    assert_eq!(tight.row_pitch, 12);
    assert_eq!(tight.row_length, 3);
    assert_eq!(tight.size, 24);

    let aligned = capture_buffer_layout(3, 2, 16).unwrap();
    assert_eq!(aligned.row_pitch, 16);
    assert_eq!(aligned.row_length, 4);
    assert_eq!(aligned.size, 32);
    assert!(capture_buffer_layout(0, 2, 16).is_err());
}
